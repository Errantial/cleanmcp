//! Platform-specific filesystem details: directory listing, allocated size,
//! file identity, devices.

use std::fs::Metadata;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::tree::{NO_TIME, NodeKind, mtime_to_u32};

/// Facts about the filesystem a scan starts on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FsInfo {
    /// Cluster (allocation unit) size in bytes. Used on Windows.
    #[allow(dead_code)]
    pub cluster: u64,
    /// Device id of the scan root. Used on Unix for `--one-filesystem`.
    #[allow(dead_code)]
    pub device: u64,
    /// Files up to this length live inside the NTFS MFT record and use no clusters.
    #[allow(dead_code)]
    pub resident_limit: u64,
}

/// A file's identity, used to count hard-linked files once.
pub(crate) type FileId = (u64, u64);

/// Modification time as Unix seconds, or [`NO_TIME`].
pub(crate) fn meta_mtime(meta: &Metadata) -> u32 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => mtime_to_u32(d.as_secs().min(i32::MAX as u64) as i64),
            // Before 1970 is stored as the epoch: age queries see "very old" either way.
            Err(_) => 0,
        },
        Err(_) => NO_TIME,
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    /// One entry from a directory listing, with the metadata that came with it.
    pub(crate) struct FsEntry<'a> {
        /// Name, lossily decoded for display and glob matching.
        pub name: &'a str,
        /// Raw name, so paths can be built without re-encoding.
        raw: &'a OsStr,
        pub kind: NodeKind,
        /// Logical size in bytes (0 for directories and links).
        pub size: u64,
        pub mtime: u32,
        meta: &'a Metadata,
    }

    impl<'a> FsEntry<'a> {
        pub(crate) fn from_meta(
            name: &'a str,
            raw: &'a OsStr,
            meta: &'a Metadata,
            mtime: u32,
        ) -> Self {
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                NodeKind::Link
            } else if ft.is_dir() {
                NodeKind::Dir
            } else if ft.is_file() {
                NodeKind::File
            } else {
                NodeKind::Other
            };
            let size = if kind == NodeKind::File {
                meta.len()
            } else {
                0
            };
            FsEntry {
                name,
                raw,
                kind,
                size,
                mtime,
                meta,
            }
        }

        /// Append this entry's name to `buf` (a directory path).
        pub(crate) fn push_name(&self, buf: &mut PathBuf) {
            buf.push(self.raw);
        }

        /// Allocated size from the listing metadata alone. `None` means only the
        /// filesystem can tell, and the caller has to ask with [`ask_fs`].
        pub(crate) fn disk_size_known(&self, _fs: &FsInfo) -> Option<u64> {
            // st_blocks is always in 512-byte units and comes with the stat.
            Some(self.meta.blocks() * 512)
        }

        /// Fallback when asking the filesystem failed.
        pub(crate) fn disk_size_estimate(&self, _fs: &FsInfo) -> u64 {
            self.meta.blocks() * 512
        }

        pub(crate) fn device(&self) -> Option<u64> {
            Some(self.meta.dev())
        }

        /// Identity of a file with more than one hard link. Cheap on Unix (comes
        /// from the stat), so `--dedupe-hardlinks` is not needed.
        pub(crate) fn hardlink_id(&self, _path: &Path, _force: bool) -> Option<FileId> {
            (self.meta.nlink() > 1).then(|| (self.meta.dev(), self.meta.ino()))
        }
    }

    /// Not needed on Unix: `st_blocks` already answered ([`FsEntry::disk_size_known`]).
    pub(crate) fn ask_fs(_path: &Path) -> Option<u64> {
        None
    }

    /// List `dir`, calling `f` for every entry (never `.`/`..`). Returns the
    /// number of entries that could not be read.
    pub(crate) fn walk_dir(dir: &Path, f: &mut impl FnMut(&FsEntry<'_>)) -> io::Result<usize> {
        let mut errors = 0;
        for entry in fs::read_dir(dir)? {
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            // `DirEntry::metadata` does not follow links (one lstat).
            let Ok(meta) = entry.metadata() else {
                errors += 1;
                continue;
            };
            let raw = entry.file_name();
            let name = raw.to_string_lossy();
            let e = FsEntry::from_meta(&name, &raw, &meta, meta_mtime(&meta));
            f(&e);
        }
        Ok(errors)
    }

    pub fn fs_info(_root: &Path, meta: &Metadata) -> FsInfo {
        FsInfo {
            cluster: 4096,
            device: meta.dev(),
            resident_limit: 0,
        }
    }

    pub fn is_virtual_fs(path: &Path) -> bool {
        // Kernel/pseudo filesystems that report fake or huge sizes.
        matches!(
            path.to_str(),
            Some("/proc" | "/sys" | "/dev" | "/run" | "/var/run")
        )
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::path::PathBuf;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
        GetCompressedFileSizeW, GetDiskFreeSpaceW, GetDriveTypeW, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING,
    };

    const DRIVE_REMOTE: u32 = 4;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
    const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    /// Allocation differs from "length rounded up to a cluster"; ask the filesystem.
    const ASK_FS: u32 = FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED;
    /// Cloud placeholders (OneDrive "online-only" etc.): data is not on this disk.
    /// Asking the filesystem is very slow for these (it goes through the cloud
    /// filter driver), so treat them as 0 bytes on disk.
    /// Note: FILE_ATTRIBUTE_RECALL_ON_OPEN (0x40000) is deliberately not used: on
    /// files it shares its value with FILE_ATTRIBUTE_EA, which ordinary files have.
    const NOT_LOCAL: u32 = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

    const NUL: u16 = 0;

    /// Entries per directory query. One call usually covers a whole small
    /// directory, so a 753k-entry scan needs ~15k syscalls instead of ~1.5M.
    const DIR_BUFFER_SIZE: usize = 64 * 1024;

    /// Conservative upper bound for NTFS "resident" files (data stored in the
    /// 1 KiB MFT record itself). The real limit is ~700 bytes minus name length.
    const NTFS_RESIDENT_LIMIT: u64 = 512;

    /// UTF-16 form of `path` without a terminator. Absolute paths get the `\\?\`
    /// prefix, which also removes the MAX_PATH limit.
    pub(crate) fn wide(path: &Path) -> Vec<u16> {
        let s = path.as_os_str();
        let bytes = s.as_encoded_bytes();
        let mut v: Vec<u16> = Vec::with_capacity(bytes.len() + 16);
        let already = bytes.starts_with(br"\\?\") || bytes.starts_with(br"\\.\");
        if !already && path.is_absolute() {
            if bytes.starts_with(br"\\") {
                // UNC: \\server\share -> \\?\UNC\server\share
                v.extend(r"\\?\UNC".encode_utf16());
                v.push(b'\\' as u16);
                v.extend(s.encode_wide().skip(2));
            } else {
                v.extend(r"\\?\".encode_utf16());
                v.extend(s.encode_wide());
            }
        } else {
            v.extend(s.encode_wide());
        }
        v
    }

    /// NUL-terminated UTF-16 path for API calls that take a file name.
    fn wide_nul(path: &Path) -> Vec<u16> {
        let mut v = wide(path);
        v.push(NUL);
        v
    }

    /// Windows FILETIME (100 ns units since 1601) to Unix seconds, truncated the
    /// same way `std::time::SystemTime` does.
    fn filetime_to_unix(ft: u64) -> i64 {
        const UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
        const PER_SEC: u64 = 10_000_000;
        if ft >= UNIX_EPOCH_100NS {
            ((ft - UNIX_EPOCH_100NS) / PER_SEC) as i64
        } else {
            -(((UNIX_EPOCH_100NS - ft) / PER_SEC) as i64)
        }
    }

    /// One entry from a directory listing, with the metadata that came with it.
    pub(crate) struct FsEntry<'a> {
        /// Name, lossily decoded for display and glob matching.
        pub name: &'a str,
        pub kind: NodeKind,
        /// Logical size in bytes (0 for directories and links).
        pub size: u64,
        pub mtime: u32,
        attrs: u32,
    }

    impl<'a> FsEntry<'a> {
        /// Entry straight from a `FILE_FULL_DIR_INFO` record. Reparse points cannot
        /// be classified from this data (the record has no reparse tag), so the
        /// caller resolves those with [`FsEntry::from_meta`].
        fn from_batch(attrs: u32, write_time: i64, size: i64, name: &'a str) -> Self {
            let kind = if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                NodeKind::Dir
            } else {
                NodeKind::File
            };
            FsEntry {
                name,
                kind,
                size: if kind == NodeKind::File {
                    size as u64
                } else {
                    0
                },
                mtime: mtime_to_u32(filetime_to_unix(write_time as u64)),
                attrs,
            }
        }

        pub(crate) fn from_meta(
            name: &'a str,
            _raw: &'a OsStr,
            meta: &Metadata,
            mtime: u32,
        ) -> Self {
            let attrs = meta.file_attributes();
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                NodeKind::Link
            } else if ft.is_dir() {
                NodeKind::Dir
            } else {
                NodeKind::File
            };
            let size = if kind == NodeKind::File {
                meta.len()
            } else {
                0
            };
            FsEntry {
                name,
                kind,
                size,
                mtime,
                attrs,
            }
        }

        /// Append this entry's name to `buf` (a directory path).
        pub(crate) fn push_name(&self, buf: &mut PathBuf) {
            buf.push(self.name);
        }

        /// Allocated size from the listing metadata alone. `None` means only the
        /// filesystem can tell, and the caller has to ask with [`ask_fs`].
        pub(crate) fn disk_size_known(&self, fs: &FsInfo) -> Option<u64> {
            if self.attrs & NOT_LOCAL != 0 {
                return Some(0);
            }
            if self.attrs & ASK_FS != 0 {
                return None;
            }
            Some(self.disk_size_estimate(fs))
        }

        /// Length rounded up to whole clusters. Also the fallback when asking the
        /// filesystem failed.
        pub(crate) fn disk_size_estimate(&self, fs: &FsInfo) -> u64 {
            if self.size <= fs.resident_limit {
                return 0;
            }
            self.size.div_ceil(fs.cluster) * fs.cluster
        }

        /// Mounted volumes inside folders are reparse points, which are never
        /// followed, so a Windows scan already stays on one volume.
        pub(crate) fn device(&self) -> Option<u64> {
            None
        }

        /// Identity of a file with more than one hard link. Needs to open the file,
        /// so it only runs when `force` is set (`--dedupe-hardlinks`).
        pub(crate) fn hardlink_id(&self, path: &Path, force: bool) -> Option<FileId> {
            if !force {
                return None;
            }
            let w = wide_nul(path);
            // SAFETY: standard CreateFileW/GetFileInformationByHandle/CloseHandle
            // sequence on a valid NUL-terminated path.
            unsafe {
                let h = CreateFileW(
                    w.as_ptr(),
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    ptr::null_mut(),
                );
                if h == INVALID_HANDLE_VALUE {
                    return None;
                }
                let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
                let ok = GetFileInformationByHandle(h, &mut info) != 0;
                CloseHandle(h);
                (ok && info.nNumberOfLinks > 1).then_some((
                    info.dwVolumeSerialNumber as u64,
                    ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                ))
            }
        }
    }

    /// Bytes actually allocated to a file, straight from the filesystem. Only
    /// needed for sparse and compressed files.
    pub(crate) fn ask_fs(path: &Path) -> Option<u64> {
        let w = wide_nul(path);
        let mut high = 0u32;
        // SAFETY: valid NUL-terminated path and out-pointer.
        let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
        // u32::MAX is only an error if GetLastError says so (it can be a real low word).
        if low != u32::MAX || unsafe { GetLastError() } == NO_ERROR {
            Some(((high as u64) << 32) | low as u64)
        } else {
            None
        }
    }

    /// Open directory handle; closed on drop.
    struct DirHandle(HANDLE);

    impl Drop for DirHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Per-thread listing buffer, reused so a scan does not allocate 64 KiB per
    /// directory. `try_borrow_mut` keeps the code panic-free if it is ever re-entered.
    fn with_dir_buffer<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
        thread_local! {
            static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        }
        BUFFER.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => {
                if buf.len() < DIR_BUFFER_SIZE {
                    buf.resize(DIR_BUFFER_SIZE, 0);
                }
                f(&mut buf[..])
            }
            Err(_) => f(&mut vec![0u8; DIR_BUFFER_SIZE]),
        })
    }

    /// Open `dir` for listing. Fails only if the directory cannot be opened at all;
    /// an empty directory opens fine and yields no entries.
    fn open_dir(dir: &Path) -> io::Result<DirHandle> {
        let w = wide_nul(dir);
        // SAFETY: `w` is a NUL-terminated UTF-16 path; the handle is checked before
        // use and closed by `DirHandle`.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(DirHandle(h))
    }

    /// Walk one batch of records, calling `f` for every entry.
    fn emit_batch(
        buf: &[u8],
        name: &mut String,
        scratch: &mut PathBuf,
        f: &mut impl FnMut(&FsEntry<'_>),
    ) -> usize {
        let mut errors = 0;
        let mut off = 0usize;
        loop {
            // SAFETY: the caller guarantees `buf` holds complete records from
            // `off` on, as produced by `GetFileInformationByHandleEx`.
            let info = unsafe { buf.as_ptr().add(off).cast::<FILE_FULL_DIR_INFO>() };
            let (next, name_len, attrs, write_time, size) = unsafe {
                (
                    (&raw const (*info).NextEntryOffset).read_unaligned() as usize,
                    (&raw const (*info).FileNameLength).read_unaligned() as usize,
                    (&raw const (*info).FileAttributes).read_unaligned(),
                    (&raw const (*info).LastWriteTime).read_unaligned(),
                    (&raw const (*info).EndOfFile).read_unaligned(),
                )
            };
            // SAFETY: the record's file name occupies `name_len` bytes inside `buf`.
            let units = unsafe {
                std::slice::from_raw_parts(
                    (&raw const (*info).FileName).cast::<u16>(),
                    name_len / 2,
                )
            };
            if !matches!(units, [46] | [46, 46]) {
                name.clear();
                push_lossy_utf16(name, units);
                if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    // Listing records carry no reparse tag, and the tag is what tells
                    // a symlink/junction (never followed) from a cloud placeholder
                    // (a normal file). `std` applies that rule, so ask it.
                    scratch.push(&*name);
                    match fs::symlink_metadata(&*scratch) {
                        Ok(meta) => {
                            let e =
                                FsEntry::from_meta(name, OsStr::new(""), &meta, meta_mtime(&meta));
                            f(&e);
                        }
                        Err(_) => errors += 1,
                    }
                    scratch.pop();
                } else {
                    f(&FsEntry::from_batch(attrs, write_time, size, name));
                }
            }
            if next == 0 {
                return errors;
            }
            off += next;
        }
    }

    /// List `dir` with one `FILE_FULL_DIR_INFO` query per 64 KiB of entries.
    fn walk_dir_batch(dir: &Path, f: &mut impl FnMut(&FsEntry<'_>)) -> io::Result<usize> {
        let handle = open_dir(dir)?;
        with_dir_buffer(|buf| {
            let mut name = String::with_capacity(48);
            let mut scratch = dir.to_path_buf();
            let mut errors = 0;
            // The first call restarts the enumeration; later calls continue it.
            let mut class = FileFullDirectoryRestartInfo;
            let mut emitted = false;
            loop {
                // SAFETY: valid directory handle and a buffer of `buf.len()` bytes.
                let ok = unsafe {
                    GetFileInformationByHandleEx(
                        handle.0,
                        class,
                        buf.as_mut_ptr().cast(),
                        buf.len() as u32,
                    )
                };
                if ok == 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                        // Nothing has been handed to `f` yet, so the caller can
                        // restart this directory with the std fallback instead of
                        // losing the subtree to a quirk of the fast path.
                        if !emitted {
                            return Err(err);
                        }
                        errors += 1;
                    }
                    return Ok(errors);
                }
                class = FileFullDirectoryInfo;
                emitted = true;
                errors += emit_batch(buf, &mut name, &mut scratch, f);
            }
        })
    }

    /// UTF-16 to UTF-8, replacing unpaired surrogates like `to_string_lossy` does.
    fn push_lossy_utf16(out: &mut String, units: &[u16]) {
        if units.iter().all(|&u| u < 0x80) {
            out.reserve(units.len());
            for &u in units {
                out.push(u as u8 as char);
            }
        } else {
            for c in char::decode_utf16(units.iter().copied()) {
                out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }

    /// List `dir`, calling `f` for every entry (never `.`/`..`). Returns the
    /// number of entries that could not be read.
    pub(crate) fn walk_dir(dir: &Path, f: &mut impl FnMut(&FsEntry<'_>)) -> io::Result<usize> {
        match walk_dir_batch(dir, f) {
            Ok(errs) => Ok(errs),
            // Extremely unlikely: let `std` try the same directory so a scan never
            // loses a subtree to a quirk of the fast path.
            Err(_) => walk_dir_std(dir, f),
        }
    }

    /// `std::fs::read_dir` fallback with the same metadata (it comes from the
    /// same directory listing).
    fn walk_dir_std(dir: &Path, f: &mut impl FnMut(&FsEntry<'_>)) -> io::Result<usize> {
        let mut errors = 0;
        for entry in fs::read_dir(dir)? {
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                errors += 1;
                continue;
            };
            let raw = entry.file_name();
            let name = raw.to_string_lossy();
            let e = FsEntry::from_meta(&name, &raw, &meta, meta_mtime(&meta));
            f(&e);
        }
        Ok(errors)
    }

    pub fn fs_info(root: &Path, _meta: &Metadata) -> FsInfo {
        let mut info = FsInfo {
            cluster: 4096,
            device: 0,
            resident_limit: 0,
        };
        let root = wide_nul(root);
        let mut vol = [0u16; 1024];
        // SAFETY: buffers are valid for the lengths passed; unused out-params are null.
        unsafe {
            if GetVolumePathNameW(root.as_ptr(), vol.as_mut_ptr(), vol.len() as u32) == 0 {
                return info;
            }
            let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
            if GetDiskFreeSpaceW(vol.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) != 0
                && spc > 0
                && bps > 0
            {
                info.cluster = spc as u64 * bps as u64;
            }
            let mut fs_name = [0u16; 64];
            if GetVolumeInformationW(
                vol.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            ) != 0
            {
                let len = fs_name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(fs_name.len());
                // A share reports the server's filesystem name, but residency is a
                // local-MFT fact: a remote file is never in our MFT, and the space it
                // occupies is not local disk space either.
                let remote = GetDriveTypeW(vol.as_ptr()) == DRIVE_REMOTE;
                if !remote && String::from_utf16_lossy(&fs_name[..len]) == "NTFS" {
                    info.resident_limit = NTFS_RESIDENT_LIMIT;
                }
            }
        }
        info
    }

    pub fn is_virtual_fs(_path: &Path) -> bool {
        false
    }
}

pub(crate) use imp::*;

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn virtual_fs_paths() {
        assert!(super::is_virtual_fs(std::path::Path::new("/proc")));
        assert!(!super::is_virtual_fs(std::path::Path::new("/home/proc")));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    #[test]
    fn walk_dir_matches_read_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), b"12345").unwrap();
        fs::write(dir.path().join("sub/b.txt"), vec![1u8; 5000]).unwrap();
        fs::write(dir.path().join(".hidden"), b"x").unwrap();

        let mut names = Vec::new();
        let mut kinds = Vec::new();
        let errors = walk_dir(dir.path(), &mut |e| {
            names.push(e.name.to_string());
            kinds.push(e.kind);
            assert!(e.name != "." && e.name != "..");
        })
        .unwrap();

        assert_eq!(errors, 0);
        names.sort();
        assert_eq!(names, [".hidden", "a.txt", "sub"]);
        assert_eq!(kinds.iter().filter(|k| **k == NodeKind::Dir).count(), 1);
        assert_eq!(names.len(), fs::read_dir(dir.path()).unwrap().count());
    }

    #[test]
    fn walk_dir_reports_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sized.bin");
        fs::write(&file, vec![7u8; 9000]).unwrap();
        let meta = fs::symlink_metadata(&file).unwrap();

        let mut seen = 0;
        walk_dir(dir.path(), &mut |e| {
            if e.name == "sized.bin" {
                seen += 1;
                assert_eq!(e.kind, NodeKind::File);
                assert_eq!(e.size, 9000);
                assert_eq!(e.mtime, meta_mtime(&meta));
            }
        })
        .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn walk_dir_of_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut count = 0;
        assert_eq!(walk_dir(dir.path(), &mut |_| count += 1).unwrap(), 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn wide_prefixes_are_well_formed() {
        let w = |p: &str| String::from_utf16(&imp::wide(std::path::Path::new(p))).unwrap();
        assert_eq!(w(r"C:\x"), r"\\?\C:\x");
        // UNC keeps the separator before the server name.
        assert_eq!(w(r"\\server\share\dir"), r"\\?\UNC\server\share\dir");
        // Already-verbatim paths are left alone.
        assert_eq!(w(r"\\?\C:\x"), r"\\?\C:\x");
    }

    /// Junctions must be links: `scan_dir` never descends into a `Link`, which is
    /// what keeps a profile containing `Application Data -> AppData\Roaming`
    /// from looping. `mklink /J` needs no elevation.
    #[test]
    fn junction_is_a_link_and_is_not_descended() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("loop");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: this filesystem does not support junctions");
            return;
        }
        let mut kind = None;
        walk_dir(dir.path(), &mut |e| {
            if e.name == "loop" {
                kind = Some(e.kind);
            }
        })
        .unwrap();
        assert_eq!(kind, Some(NodeKind::Link));

        let s = crate::scan(dir.path(), &crate::ScanOptions::default()).unwrap();
        assert_eq!(
            s.find(std::path::Path::new("loop")).unwrap().kind(),
            NodeKind::Link
        );
        assert_eq!(s.tree.dir_count(), 0, "junction was descended into");
    }
}
