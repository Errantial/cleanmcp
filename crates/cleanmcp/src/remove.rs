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
    move_to_trash_checked(paths, &|_| Ok(()))
}

/// Like [`move_to_trash`], but `check` is applied to the exact path that will be
/// deleted. The parent folder is resolved on both sides of the call, so a
/// caller that checked an earlier resolution can confirm they still agree.
pub fn move_to_trash_checked(
    paths: &[PathBuf],
    check: &dyn Fn(&Path) -> Result<(), String>,
) -> Vec<Removal> {
    paths.iter().map(|p| trash_one(p, check)).collect()
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

fn send_to_trash(target: &Path) -> Result<(), String> {
    // Debug builds only: tests set CLEANMCP_TEST_TRASH_DIR so end-to-end runs do not
    // fill the real Recycle Bin. Release builds always use the real trash.
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("CLEANMCP_TEST_TRASH_DIR") {
        let name = target.file_name().ok_or("no file name")?;
        let dest = Path::new(&dir).join(name);
        if dest.symlink_metadata().is_ok() {
            return Err(format!("test trash already has {}", dest.display()));
        }
        return std::fs::rename(target, dest).map_err(|e| e.to_string());
    }
    trash::delete(target).map_err(|e| e.to_string())
}

fn trash_one(path: &Path, check: &dyn Fn(&Path) -> Result<(), String>) -> Removal {
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
    // The caller's allow-list is re-applied to the path we are about to remove,
    // not to the one it checked before this call.
    if let Err(e) = check(&target) {
        return fail(e);
    }
    match send_to_trash(&target) {
        Ok(()) => Removal {
            path: path.to_path_buf(),
            ok: true,
            error: None,
        },
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_protected_paths() {
        // Never hand a real path to the trash here: if the guard regressed this
        // test would bin the developer's home folder instead of failing.
        let home = dirs_home();
        let target = deletion_target(&home).expect("home is deletable by shape");
        assert!(
            protection_reason(&target).is_some(),
            "home folder is not protected"
        );
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
        // The parent resolves to /usr/bin, so the target is refused. Check the
        // predicate directly: calling the trash here would move the real binary
        // if the guard ever regressed.
        let target = deletion_target(&dir.path().join("l/bin/sh")).unwrap();
        assert!(target.starts_with("/usr/bin"), "{}", target.display());
        assert!(
            protection_reason(&target).is_some(),
            "{} is not protected",
            target.display()
        );
        // A dangling link can be targeted (checked, not resolved).
        std::os::unix::fs::symlink("/nonexistent", dir.path().join("dangling")).unwrap();
        assert!(deletion_target(&dir.path().join("dangling")).is_ok());
    }

    /// The caller's allow-list runs against the path that is actually removed.
    #[test]
    fn checked_delete_honours_the_caller_check() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("keep-me.txt");
        std::fs::write(&f, b"data").unwrap();
        let seen: std::cell::RefCell<Vec<PathBuf>> = Default::default();
        let r = move_to_trash_checked(std::slice::from_ref(&f), &|target| {
            seen.borrow_mut().push(target.to_path_buf());
            Err("refused: changed after the check".into())
        });
        assert!(!r[0].ok);
        assert!(
            r[0].error.as_deref().unwrap().contains("changed after"),
            "{:?}",
            r[0]
        );
        assert!(f.exists(), "a refused path must not be removed");
        // The check saw the resolved target, not the raw argument.
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].ends_with("keep-me.txt"));
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
