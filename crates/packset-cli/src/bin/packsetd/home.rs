//! Where the pack lives on disk.
//!
//! Cards are files and atoms are a database, and both hang off one home so a
//! seat has exactly one place to back up. A workspace becomes a directory
//! through its slug, which is why a name carrying slashes does not nest.

use std::path::{Path, PathBuf};

use packset_core::identity::workspace_slug;

/// The root of one seat's pack.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// A home at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default home, `~/.grokinside/memory`.
    #[must_use]
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
        home.join(".grokinside").join("memory")
    }

    /// The root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The atom database.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.root.join("memory.lmdb")
    }

    /// The search projection.
    #[must_use]
    pub fn milli_dir(&self) -> PathBuf {
        self.root.join("memory.milli")
    }

    /// The single-writer lock.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("packsetd.lock")
    }

    /// One workspace's directory.
    #[must_use]
    pub fn workspace_dir(&self, workspace: &str) -> PathBuf {
        self.root.join("workspaces").join(workspace_slug(workspace))
    }

    /// The seat-wide card.
    #[must_use]
    pub fn user_path(&self) -> PathBuf {
        self.root.join("USER.md")
    }

    /// One workspace's card.
    #[must_use]
    pub fn memory_path(&self, workspace: &str) -> PathBuf {
        self.workspace_dir(workspace).join("MEMORY.md")
    }

    /// The append-only log, kept for readers that predate the database.
    #[must_use]
    pub fn atoms_path(&self, workspace: &str) -> PathBuf {
        self.workspace_dir(workspace).join("atoms.jsonl")
    }

    /// Where an overflowing card is archived, one file per day.
    #[must_use]
    pub fn archive_path(&self, workspace: &str, day: &str) -> PathBuf {
        self.workspace_dir(workspace)
            .join("archive")
            .join(format!("{day}.md"))
    }

    /// The active set for a workspace.
    #[must_use]
    pub fn pin_path(&self, workspace: &str) -> PathBuf {
        self.workspace_dir(workspace).join("pin")
    }

    /// One set's directory inside a workspace.
    #[must_use]
    pub fn set_dir(&self, workspace: &str, name: &str) -> PathBuf {
        self.workspace_dir(workspace).join("sets").join(name)
    }

    /// A set's own card.
    #[must_use]
    pub fn set_user_path(&self, workspace: &str, name: &str) -> PathBuf {
        self.set_dir(workspace, name).join("USER.md")
    }

    /// A set's own workspace card.
    #[must_use]
    pub fn set_memory_path(&self, workspace: &str, name: &str) -> PathBuf {
        self.set_dir(workspace, name).join("MEMORY.md")
    }

    /// A set's standing instructions.
    #[must_use]
    pub fn set_instructions_path(&self, workspace: &str, name: &str) -> PathBuf {
        self.set_dir(workspace, name).join("INSTRUCTIONS.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_name_becomes_one_directory() {
        let home = Home::new("/tmp/pack");
        let dir = home.workspace_dir("git:github.com/HaoZeke/vissue");
        assert_eq!(
            dir,
            PathBuf::from("/tmp/pack/workspaces/git_github.com_HaoZeke_vissue")
        );
        // The slug is what keeps a slash in the name from nesting.
        assert_eq!(dir.components().count(), 5);
    }

    #[test]
    fn the_seat_card_is_above_the_workspaces() {
        let home = Home::new("/tmp/pack");
        assert_eq!(home.user_path(), PathBuf::from("/tmp/pack/USER.md"));
        assert!(home
            .memory_path("global")
            .starts_with("/tmp/pack/workspaces"));
    }

    #[test]
    fn a_set_lives_under_its_workspace() {
        let home = Home::new("/tmp/pack");
        assert_eq!(
            home.set_user_path("global", "review"),
            PathBuf::from("/tmp/pack/workspaces/global/sets/review/USER.md")
        );
    }
}
