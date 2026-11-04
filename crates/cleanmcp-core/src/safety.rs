//! Paths that must never be deleted, whoever asks (TUI or MCP).

use std::path::{Path, PathBuf};

/// Normalize for comparison: drop the `\\?\` verbatim prefix, `/` separators,
/// no trailing slash, lowercase on Windows.
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    };
    let s = s.replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn is_same_or_inside(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && path[dir.len()..].starts_with('/'))
}

/// Everything at or below these is off limits.
#[cfg(windows)]
fn system_dirs() -> Vec<PathBuf> {
    [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .chain(
        [
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ]
        .map(PathBuf::from),
    )
    .collect()
}

#[cfg(not(windows))]
fn system_dirs() -> Vec<PathBuf> {
    [
        "/bin",
        "/boot",
        "/dev",
        "/etc",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/proc",
        "/run",
        "/sbin",
        "/sys",
        "/usr",
        "/snap",
        "/var/lib",
        "/var/log",
        "/var/spool",
    ]
    .map(PathBuf::from)
    .to_vec()
}

/// Folders holding user profiles: only paths inside *your own* profile are allowed.
#[cfg(windows)]
fn profile_roots() -> Vec<PathBuf> {
    vec![PathBuf::from(r"C:\Users")]
}

#[cfg(not(windows))]
fn profile_roots() -> Vec<PathBuf> {
    ["/home", "/root"].map(PathBuf::from).to_vec()
}

/// Folders that may hold deletable things but must not be deleted themselves.
#[cfg(windows)]
fn container_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(windows))]
fn container_dirs() -> Vec<PathBuf> {
    [
        "/var",
        "/opt",
        "/srv",
        "/mnt",
        "/media",
        "/tmp",
        "/var/cache",
        "/var/tmp",
    ]
    .map(PathBuf::from)
    .to_vec()
}

/// Why `path` must not be deleted, or `None` if deleting it is allowed.
/// `path` should be absolute with its parent resolved (see `resolve_path`).
pub fn protection_reason(path: &Path) -> Option<&'static str> {
    protection_reason_for(path, dirs::home_dir().as_deref())
}

fn protection_reason_for(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    if !path.is_absolute() {
        return Some("path is not absolute");
    }
    let p = norm(path);
    if path.parent().is_none() || !p.contains('/') || p.ends_with(':') {
        return Some("it is a drive or filesystem root");
    }
    // Profiles are not always under a fixed root: Windows supports relocated
    // profiles (D:\Profiles\me) and Silverblue uses /var/home/me. The parent of
    // the real home is the profile root in those layouts, as long as it is not
    // itself a filesystem root.
    let profile_parent = home
        .and_then(Path::parent)
        .filter(|parent| parent.parent().is_some())
        .map(norm);
    let home = home.map(norm);
    if let Some(h) = &home
        && is_same_or_inside(h, &p)
    {
        return Some("it is your home folder or contains it");
    }
    if let Some(parent) = &profile_parent
        && is_same_or_inside(&p, parent)
        && !home.as_deref().is_some_and(|h| is_same_or_inside(&p, h))
    {
        return Some("it belongs to another user profile");
    }
    if system_dirs()
        .iter()
        .any(|d| is_same_or_inside(&p, &norm(d)))
    {
        return Some("it is a system folder (use the OS cleanup tool instead)");
    }
    for root in profile_roots() {
        let r = norm(&root);
        let inside_own_home = home.as_deref().is_some_and(|h| is_same_or_inside(&p, h));
        if is_same_or_inside(&p, &r) && !inside_own_home {
            return Some("it belongs to another user profile");
        }
    }
    if container_dirs().iter().any(|d| p == norm(d)) {
        return Some("it is a top-level system folder");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_roots_home_and_system() {
        let home = dirs::home_dir().unwrap();
        assert!(protection_reason(&home).is_some());
        assert!(protection_reason(home.parent().unwrap()).is_some());
        assert!(protection_reason(Path::new("relative/path")).is_some());
        assert!(protection_reason(&home.join("project").join("node_modules")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_rules() {
        let home = Path::new(r"C:\Users\me");
        let r = |p: &str| protection_reason_for(Path::new(p), Some(home));
        assert!(r(r"C:\").is_some());
        assert!(r(r"C:\Windows\Temp").is_some());
        assert!(r(r"c:\program files\App").is_some());
        assert!(r(r"\\?\C:\Windows\WinSxS\x").is_some(), "verbatim prefix");
        assert!(r(r"C:\Users\Public").is_some());
        assert!(r(r"C:\Users\other\AppData").is_some());
        assert!(r(r"C:\Users\me\AppData\Local\Temp").is_none());
        assert!(r(r"D:\games\old").is_none());

        // Relocated profiles: the parent of the real home is a profile root too.
        let relocated = Path::new(r"D:\Profiles\me");
        let r = |p: &str| protection_reason_for(Path::new(p), Some(relocated));
        assert!(r(r"D:\Profiles\other\Documents").is_some());
        assert!(r(r"D:\Profiles\me\AppData\Local\Temp").is_none());
        assert!(r(r"D:\games\old").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_rules() {
        let home = Path::new("/home/me");
        let r = |p: &str| protection_reason_for(Path::new(p), Some(home));
        assert!(r("/").is_some());
        assert!(r("/usr/lib/x").is_some());
        assert!(r("/var/lib/dpkg").is_some());
        assert!(r("/var/cache").is_some());
        assert!(r("/home/other/x").is_some());
        assert!(r("/root/.cache").is_some());
        assert!(r("/var/cache/apt/archives").is_none());
        assert!(r("/home/me/.cache/pip").is_none());
        // A similar prefix is not "inside".
        assert!(r("/usrdata/x").is_none());

        // Relocated profiles (Silverblue-style /var/home) are protected too.
        let relocated = Path::new("/var/home/me");
        let r = |p: &str| protection_reason_for(Path::new(p), Some(relocated));
        assert!(r("/var/home/other/x").is_some());
        assert!(r("/var/home/me/.cache/x").is_none());
    }
}
