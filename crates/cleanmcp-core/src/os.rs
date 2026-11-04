//! Platform-specific filesystem details: allocated size, file identity, devices.

use std::fs::Metadata;
use std::path::Path;

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

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub fn fs_info(_root: &Path, meta: &Metadata) -> FsInfo {
        FsInfo {
            cluster: 4096,
            device: meta.dev(),
            resident_limit: 0,
        }
    }

    pub fn disk_size(_path: &Path, meta: &Metadata, _fs: &FsInfo) -> u64 {
        // st_blocks is always in 512-byte units.
        meta.blocks() * 512
    }

    /// Identity of a file with more than one hard link. Cheap on Unix (comes from stat).
    pub fn hardlink_id(_path: &Path, meta: &Metadata, _force: bool) -> Option<FileId> {
        (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
    }

    pub fn device(meta: &Metadata) -> Option<u64> {
        Some(meta.dev())
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
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetCompressedFileSizeW, GetDiskFreeSpaceW, GetFileInformationByHandle,
        GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING,
    };

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

    /// NUL-terminated UTF-16 path, with the `\\?\` prefix for long paths.
    fn wide(path: &Path) -> Vec<u16> {
        let s = path.as_os_str();
        let mut v: Vec<u16> = Vec::with_capacity(s.len() + 5);
        if s.len() >= 240 && !path.to_string_lossy().starts_with(r"\\") {
            v.extend(r"\\?\".encode_utf16());
        }
        v.extend(s.encode_wide());
        v.push(0);
        v
    }

    /// Conservative upper bound for NTFS "resident" files (data stored in the
    /// 1 KiB MFT record itself). The real limit is ~700 bytes minus name length.
    const NTFS_RESIDENT_LIMIT: u64 = 512;

    pub fn fs_info(root: &Path, _meta: &Metadata) -> FsInfo {
        let mut info = FsInfo {
            cluster: 4096,
            device: 0,
            resident_limit: 0,
        };
        let mut vol = [0u16; 1024];
        // SAFETY: buffers are valid for the lengths passed; unused out-params are null.
        unsafe {
            if GetVolumePathNameW(wide(root).as_ptr(), vol.as_mut_ptr(), vol.len() as u32) == 0 {
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
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            ) != 0
            {
                let len = fs_name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(fs_name.len());
                if String::from_utf16_lossy(&fs_name[..len]) == "NTFS" {
                    info.resident_limit = NTFS_RESIDENT_LIMIT;
                }
            }
        }
        info
    }

    pub fn disk_size(path: &Path, meta: &Metadata, fs: &FsInfo) -> u64 {
        let attrs = meta.file_attributes();
        if attrs & NOT_LOCAL != 0 {
            return 0;
        }
        if attrs & ASK_FS != 0 {
            let mut high = 0u32;
            // SAFETY: valid NUL-terminated path and out-pointer.
            let low = unsafe { GetCompressedFileSizeW(wide(path).as_ptr(), &mut high) };
            // u32::MAX is only an error if GetLastError says so (it can be a real low word).
            if low != u32::MAX || unsafe { GetLastError() } == NO_ERROR {
                return ((high as u64) << 32) | low as u64;
            }
        }
        let len = meta.len();
        if len <= fs.resident_limit {
            return 0;
        }
        len.div_ceil(fs.cluster) * fs.cluster
    }

    /// Identity of a file with more than one hard link. Needs to open the file,
    /// so it only runs when `force` is set (`--dedupe-hardlinks`).
    pub fn hardlink_id(path: &Path, _meta: &Metadata, force: bool) -> Option<FileId> {
        if !force {
            return None;
        }
        // SAFETY: standard CreateFileW/GetFileInformationByHandle/CloseHandle sequence.
        unsafe {
            let h = CreateFileW(
                wide(path).as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            let ok = GetFileInformationByHandle(h, &mut info) != 0;
            CloseHandle(h);
            (ok && info.nNumberOfLinks > 1).then_some({
                (
                    info.dwVolumeSerialNumber as u64,
                    ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                )
            })
        }
    }

    /// Mounted volumes inside folders are reparse points, which are never followed,
    /// so a Windows scan already stays on one volume.
    pub fn device(_meta: &Metadata) -> Option<u64> {
        None
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
