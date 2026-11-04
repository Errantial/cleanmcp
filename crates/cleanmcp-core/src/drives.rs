//! Mounted volumes with total and free space.

use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Drive {
    /// Where the volume is mounted, e.g. `C:\` or `/home`.
    pub mount_point: PathBuf,
    /// Volume label / device name.
    pub name: String,
    pub file_system: String,
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub removable: bool,
}

/// Filesystems that hold no user data worth cleaning (Linux snaps, overlays, ...).
const PSEUDO_FS: &[&str] = &[
    "squashfs",
    "overlay",
    "tmpfs",
    "devtmpfs",
    "ramfs",
    "efivarfs",
    "fuse.snapfuse",
];

/// List mounted volumes, largest first. Pseudo filesystems are skipped unless `all` is set.
pub fn list_drives(all: bool) -> Vec<Drive> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut out: Vec<Drive> = disks
        .list()
        .iter()
        .map(|d| {
            let fs = d.file_system().to_string_lossy().into_owned();
            Drive {
                mount_point: d.mount_point().to_path_buf(),
                name: d.name().to_string_lossy().into_owned(),
                used: d.total_space().saturating_sub(d.available_space()),
                total: d.total_space(),
                available: d.available_space(),
                removable: d.is_removable(),
                file_system: fs,
            }
        })
        .filter(|d| all || (d.total > 0 && !PSEUDO_FS.contains(&d.file_system.as_str())))
        .collect();
    out.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then(a.mount_point.cmp(&b.mount_point))
    });
    if cfg!(unix) {
        // The same device can be mounted several times (bind mounts); keep the first.
        out.dedup_by(|a, b| !a.name.is_empty() && a.name == b.name && a.total == b.total);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_at_least_one_drive() {
        let d = super::list_drives(false);
        assert!(!d.is_empty(), "no drives found");
        assert!(d.iter().all(|d| d.used <= d.total));
    }
}
