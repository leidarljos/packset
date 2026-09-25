//! `USER.md` and `MEMORY.md`: the parts of the pack a person edits.
//!
//! A card is a file because a person reads and edits it, and its cap is the
//! whole point: prose past the cap is an error rather than a truncation, so
//! nobody loses a sentence to a silent trim. Overflow archives the text and
//! then refuses, which is what gives the miner something to work on.

use std::fs;
use std::io;
use std::path::Path;

use packset_core::prose;
use packset_core::record::{reject_unsafe, AtomError};

/// A card would exceed its cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overflow(pub String);

impl std::fmt::Display for Overflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Overflow {}

/// Why a card could not be written.
#[derive(Debug)]
pub enum WriteError {
    /// Past the character cap.
    Overflow(Overflow),
    /// Unsafe text, or prose too complex for the working core.
    Refused(AtomError),
    /// The filesystem said no.
    Io(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overflow(e) => write!(f, "{e}"),
            Self::Refused(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<AtomError> for WriteError {
    fn from(err: AtomError) -> Self {
        Self::Refused(err)
    }
}

/// A card's text, or empty when it is not there yet.
#[must_use]
pub fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Whether this is a pack-home card rather than a set-scoped one.
///
/// The two pack-home cards are held read-only between writes, so an agent that
/// wandered into the directory cannot edit the seat's own memory in place. A
/// set's cards are ordinary files.
fn is_pack_home_card(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match name {
        "USER.md" => !path
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new("workspaces")),
        "MEMORY.md" => {
            path.parent()
                .and_then(Path::parent)
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("workspaces")
        }
        _ => false,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Write a card, refusing text past `cap`.
///
/// # Errors
///
/// [`WriteError::Overflow`] past the cap, [`WriteError::Refused`] for unsafe or
/// too-complex prose, [`WriteError::Io`] when the write fails.
pub fn write_capped(path: &Path, text: &str, cap: usize) -> Result<(), WriteError> {
    reject_unsafe(text)?;
    let length = text.chars().count();
    if length > cap {
        let already = read_text(path).chars().count();
        let remaining = cap.saturating_sub(already);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("card");
        return Err(WriteError::Overflow(Overflow(format!(
            "{name} is {length} characters; cap is {cap}; room left on disk was {remaining}"
        ))));
    }
    if !text.trim().is_empty() {
        prose::refuse(text, prose::Role::File).map_err(AtomError::from)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pack_card = is_pack_home_card(path);
    if pack_card {
        set_mode(path, 0o644);
    }
    let written = fs::write(path, text);
    if pack_card {
        set_mode(path, 0o444);
    }
    written?;
    Ok(())
}

/// Paragraphs, which is what a card is a list of.
fn entries(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                out.push(current.trim().to_string());
            }
            current.clear();
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Append one entry, doing nothing when the card already carries it.
///
/// # Errors
///
/// As [`write_capped`].
pub fn add_entry(path: &Path, entry: &str, cap: usize) -> Result<(), WriteError> {
    let entry = entry.trim();
    reject_unsafe(entry)?;
    let current = read_text(path);
    if !entry.is_empty() && current.contains(entry) {
        return Ok(());
    }
    let mut parts = entries(&current);
    parts.push(entry.to_string());
    let joined = parts.join("\n\n") + "\n";
    write_capped(path, &joined, cap)
}

/// Replace the one entry carrying `needle`.
///
/// # Errors
///
/// [`WriteError::Refused`] unless exactly one entry matches, else as
/// [`write_capped`].
pub fn replace_entry(
    path: &Path,
    needle: &str,
    replacement: &str,
    cap: usize,
) -> Result<(), WriteError> {
    let replacement = replacement.trim();
    reject_unsafe(replacement)?;
    let current = read_text(path);
    let hits: Vec<String> = entries(&current)
        .into_iter()
        .filter(|e| e.contains(needle))
        .collect();
    if hits.len() != 1 {
        return Err(AtomError(format!(
            "replace needs exactly one match; found {}",
            hits.len()
        ))
        .into());
    }
    let updated = current.replacen(&hits[0], replacement, 1);
    write_capped(path, &updated, cap)
}

/// Drop the one entry carrying `needle`.
///
/// # Errors
///
/// [`WriteError::Refused`] unless exactly one entry matches, else as
/// [`write_capped`].
pub fn remove_entry(path: &Path, needle: &str, cap: usize) -> Result<(), WriteError> {
    let current = read_text(path);
    let all = entries(&current);
    let hits: Vec<&String> = all.iter().filter(|e| e.contains(needle)).collect();
    if hits.len() != 1 {
        return Err(AtomError(format!(
            "remove needs exactly one match; found {}",
            hits.len()
        ))
        .into());
    }
    let gone = hits[0].clone();
    let kept: Vec<String> = all.into_iter().filter(|e| *e != gone).collect();
    let joined = if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n\n") + "\n"
    };
    write_capped(path, &joined, cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use packset_core::record::{MEMORY_CAP, USER_CAP};

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_missing_card_reads_as_empty() {
        assert_eq!(read_text(Path::new("/nowhere/at/all/USER.md")), "");
    }

    #[test]
    fn past_the_cap_is_an_error_and_not_a_trim() {
        let dir = temp();
        let path = dir.path().join("USER.md");
        let long = "a".repeat(USER_CAP + 1);
        let err = write_capped(&path, &long, USER_CAP).unwrap_err();
        assert!(matches!(err, WriteError::Overflow(_)), "{err}");
        // Nothing was written, so the reader never sees a half claim.
        assert_eq!(read_text(&path), "");
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let dir = temp();
        let path = dir.path().join("MEMORY.md");
        let wide = "\u{4e00}".repeat(MEMORY_CAP);
        assert!(write_capped(&path, &wide, MEMORY_CAP).is_ok());
        let over = "\u{4e00}".repeat(MEMORY_CAP + 1);
        assert!(write_capped(&path, &over, MEMORY_CAP).is_err());
    }

    #[test]
    fn a_pack_home_card_is_read_only_between_writes() {
        let dir = temp();
        let path = dir.path().join("USER.md");
        write_capped(&path, "A standing preference.\n", USER_CAP).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o444, "{mode:o}");
        }
        // The writer still owns it: a second write goes through.
        write_capped(&path, "A different preference.\n", USER_CAP).unwrap();
        assert_eq!(read_text(&path), "A different preference.\n");
    }

    #[test]
    fn a_set_card_is_not_locked() {
        let dir = temp();
        let path = dir.path().join("workspaces/global/sets/review/USER.md");
        write_capped(&path, "Scoped to the set.\n", USER_CAP).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_ne!(mode, 0o444, "a set card is an ordinary file");
        }
    }

    #[test]
    fn adding_the_same_entry_twice_is_a_no_op() {
        let dir = temp();
        let path = dir.path().join("MEMORY.md");
        add_entry(&path, "No thanks.", MEMORY_CAP).unwrap();
        add_entry(&path, "No thanks.", MEMORY_CAP).unwrap();
        assert_eq!(read_text(&path).matches("No thanks.").count(), 1);
    }

    #[test]
    fn replace_and_remove_need_exactly_one_match() {
        let dir = temp();
        let path = dir.path().join("MEMORY.md");
        add_entry(&path, "Alpha note", MEMORY_CAP).unwrap();
        add_entry(&path, "Beta note", MEMORY_CAP).unwrap();

        // "note" matches both, so neither verb can know which was meant.
        assert!(replace_entry(&path, "note", "Gamma", MEMORY_CAP).is_err());
        assert!(remove_entry(&path, "note", MEMORY_CAP).is_err());

        replace_entry(&path, "Alpha", "Gamma note", MEMORY_CAP).unwrap();
        let text = read_text(&path);
        assert!(text.contains("Gamma note"), "{text}");
        assert!(!text.contains("Alpha note"), "{text}");

        remove_entry(&path, "Beta", MEMORY_CAP).unwrap();
        assert!(!read_text(&path).contains("Beta"));
    }

    #[test]
    fn removing_the_last_entry_leaves_an_empty_card() {
        let dir = temp();
        let path = dir.path().join("MEMORY.md");
        add_entry(&path, "Only one", MEMORY_CAP).unwrap();
        remove_entry(&path, "Only", MEMORY_CAP).unwrap();
        assert_eq!(read_text(&path), "");
    }

    #[test]
    fn a_credential_never_reaches_the_card() {
        let dir = temp();
        let path = dir.path().join("USER.md");
        let err = write_capped(&path, "api_key=abcd1234efgh", USER_CAP).unwrap_err();
        assert!(matches!(err, WriteError::Refused(_)), "{err}");
        assert_eq!(read_text(&path), "");
    }

    #[test]
    fn paragraphs_are_the_unit() {
        let text = "First one.\nstill first\n\n  \n\nSecond one.\n";
        assert_eq!(
            entries(text),
            vec![
                "First one.\nstill first".to_string(),
                "Second one.".to_string()
            ]
        );
        assert!(entries("   \n\n  ").is_empty());
    }
}
