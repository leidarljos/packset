//! Where a seat's pack lives when nothing names it.
//!
//! The pack home used to be `~/.grokinside/memory`, a name left from the
//! project's first life. It is now `$XDG_DATA_HOME/packset`
//! (`~/.local/share/packset`). A seat that already has the old directory and
//! not the new one keeps using the old one until `packset migrate-home` moves
//! it, so an upgrade never starts an empty pack beside a full one.

use std::path::{Path, PathBuf};

/// Which default a seat resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultHome {
    /// The XDG path: it exists, or neither path does.
    Current(PathBuf),
    /// The old path, because it exists and the XDG one does not.
    Legacy(PathBuf),
}

impl DefaultHome {
    /// The directory either way.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Current(p) | Self::Legacy(p) => p,
        }
    }
}

/// The home a variable names: `PACKSET_HOME`, `GROKINSIDE_HOME`, or
/// `GROK_INSIDE_MEMORY_HOME`, the first one set and not empty.
#[must_use]
pub fn named_home() -> Option<PathBuf> {
    ["PACKSET_HOME", "GROKINSIDE_HOME", "GROK_INSIDE_MEMORY_HOME"]
        .iter()
        .find_map(|key| std::env::var_os(key).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// `$XDG_DATA_HOME/packset`, else `~/.local/share/packset`.
#[must_use]
pub fn current_home() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    xdg.or_else(|| user_home().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("packset"))
}

/// `~/.grokinside/memory`.
#[must_use]
pub fn legacy_home() -> Option<PathBuf> {
    user_home().map(|h| h.join(".grokinside").join("memory"))
}

/// The default when no variable names a home. See the module text.
#[must_use]
pub fn default_home() -> Option<DefaultHome> {
    let current = current_home()?;
    Some(pick(current, legacy_home()))
}

/// The variable's home if one is set, else [`default_home`].
#[must_use]
pub fn resolve() -> Option<PathBuf> {
    named_home().or_else(|| default_home().map(|d| d.path().to_path_buf()))
}

fn pick(current: PathBuf, legacy: Option<PathBuf>) -> DefaultHome {
    match legacy {
        Some(old) if !current.exists() && old.is_dir() => DefaultHome::Legacy(old),
        _ => DefaultHome::Current(current),
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Move `old` to `new` and leave a symlink at `old`, so a client that still
/// looks there finds the token. Refuses when `new` exists, `old` is missing
/// or already a symlink, or the two sit on different filesystems.
///
/// # Errors
///
/// Returns the reason it did nothing, or the I/O error that stopped it.
pub fn migrate(old: &Path, new: &Path) -> Result<(), String> {
    if new.exists() {
        return Err(format!("{} already exists; nothing moved", new.display()));
    }
    match std::fs::symlink_metadata(old) {
        Err(_) => return Err(format!("no pack at {}; nothing to move", old.display())),
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!("{} is already a link; nothing moved", old.display()))
        }
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a directory", old.display())),
        Ok(_) => {}
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::rename(old, new).map_err(|e| {
        format!(
            "could not move {} to {}: {e}; on two filesystems, move it by hand and set PACKSET_HOME",
            old.display(),
            new.display()
        )
    })?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(new, old).map_err(|e| {
        format!(
            "moved to {}, but could not link {} to it: {e}",
            new.display(),
            old.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("packset-home-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_old_pack_wins_only_while_the_new_path_is_absent() {
        let dir = scratch("pick");
        let old = dir.join(".grokinside/memory");
        let new = dir.join("share/packset");
        assert_eq!(pick(new.clone(), Some(old.clone())), DefaultHome::Current(new.clone()));
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(pick(new.clone(), Some(old.clone())), DefaultHome::Legacy(old.clone()));
        std::fs::create_dir_all(&new).unwrap();
        assert_eq!(pick(new.clone(), Some(old)), DefaultHome::Current(new));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_moves_the_pack_and_links_the_old_name() {
        let dir = scratch("move");
        let old = dir.join(".grokinside/memory");
        let new = dir.join("share/packset");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("token"), "t\n").unwrap();
        migrate(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("token")).unwrap(), "t\n");
        assert!(std::fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(old.join("token")).unwrap(), "t\n");
        assert!(pick(new.clone(), Some(old.clone())) == DefaultHome::Current(new.clone()));
        let again = migrate(&old, &new).unwrap_err();
        assert!(again.contains("already exists"), "{again}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_refuses_without_an_old_pack() {
        let dir = scratch("none");
        let err = migrate(&dir.join("missing"), &dir.join("new")).unwrap_err();
        assert!(err.contains("nothing to move"), "{err}");
        assert!(!dir.join("new").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
