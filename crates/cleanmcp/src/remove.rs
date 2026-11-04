//! Moving files to the Recycle Bin / Trash. The only place cleanmcp deletes anything.

use std::path::{Path, PathBuf};

use cleanmcp_core::safety::protection_reason;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Removal {
    pub path: PathBuf,
    pub ok: bool,
    /// Why it failed or was refused.
    pub error: Option<String>,
}

/// Move each path to the Recycle Bin / Trash. Protected paths are refused.
/// Never deletes permanently.
pub fn move_to_trash(paths: &[PathBuf]) -> Vec<Removal> {
    paths.iter().map(|p| trash_one(p)).collect()
}

/// The exact path that will be deleted: parent folder resolved (links, `..`, case),
/// last component kept as-is so a symlink is removed, never its target.
/// This mirrors what the `trash` crate does internally, so the path we check is
/// the path that gets deleted.
pub fn deletion_target(path: &Path) -> Result<PathBuf, String> {
    let abs = std::path::absolute(path).map_err(|e| format!("bad path: {e}"))?;
    let name = abs.file_name().ok_or("it is a drive or filesystem root")?;
    if name == ".." || name == "." {
        return Err("path must not end in . or ..".into());
    }
    let parent = abs.parent().ok_or("it is a drive or filesystem root")?;
    let parent = cleanmcp_core::resolve_path(parent)
        .map_err(|e| format!("cannot resolve parent folder: {e}"))?;
    let target = parent.join(name);
    target
        .symlink_metadata()
        .map_err(|e| format!("cannot access: {e}"))?;
    Ok(target)
}

fn trash_one(path: &Path) -> Removal {
    let fail = |e: String| Removal {
        path: path.to_path_buf(),
        ok: false,
        error: Some(e),
    };
    let target = match deletion_target(path) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    if let Some(reason) = protection_reason(&target) {
        return fail(format!("refused: {reason}"));
    }
    match trash::delete(&target) {
        Ok(()) => Removal {
            path: path.to_path_buf(),
            ok: true,
            error: None,
        },
        Err(e) => fail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_protected_paths() {
        let home = dirs_home();
        let r = move_to_trash(std::slice::from_ref(&home));
        assert!(!r[0].ok);
        assert!(r[0].error.as_deref().unwrap().starts_with("refused"));
        assert!(home.exists());
    }

    fn dirs_home() -> PathBuf {
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .expect("home dir")
    }

    #[cfg(unix)]
    #[test]
    fn link_into_system_folder_is_checked_by_real_location() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr", dir.path().join("l")).unwrap();
        // The parent resolves to /usr/bin, so this is refused.
        let r = move_to_trash(&[dir.path().join("l/bin/sh")]);
        assert!(
            r[0].error.as_deref().unwrap().starts_with("refused"),
            "{:?}",
            r[0]
        );
        // A dangling link can be targeted (checked, not resolved).
        std::os::unix::fs::symlink("/nonexistent", dir.path().join("dangling")).unwrap();
        assert!(deletion_target(&dir.path().join("dangling")).is_ok());
    }

    /// Moves a small temp file to the real Recycle Bin / Trash. Run with `--ignored`.
    #[test]
    #[ignore]
    fn really_moves_to_trash() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("cleanmcp-trash-test.txt");
        std::fs::write(&f, b"cleanmcp test file, safe to delete").unwrap();
        let r = move_to_trash(std::slice::from_ref(&f));
        assert!(r[0].ok, "{:?}", r[0].error);
        assert!(!f.exists());
    }
}
