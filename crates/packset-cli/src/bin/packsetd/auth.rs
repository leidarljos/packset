//! Who may speak to the writer.
//!
//! The socket is loopback, and every user on the host can reach loopback.
//! The writer keeps a random token in `{home}/token`, readable by its owner
//! alone, and answers 401 to any request but `GET /health` that does not
//! carry it as `Authorization: Bearer TOKEN`. A client run by the same user
//! reads the same file.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The token file's name in a pack home.
pub const TOKEN_FILE: &str = "token";

/// Bytes of randomness in a token; it is written as twice as many hex digits.
const TOKEN_BYTES: usize = 32;

/// The token file under `root`.
#[must_use]
pub fn token_path(root: &Path) -> PathBuf {
    root.join(TOKEN_FILE)
}

/// The writer's token: the one in `{root}/token` when it is well formed,
/// else a fresh one written there. The file is left at mode 0600; one owned
/// by a user other than the home's owner is refused.
///
/// # Errors
///
/// Fails when the file cannot be read or written, belongs to another user,
/// or no randomness can be read.
pub fn ensure_token(root: &Path) -> anyhow::Result<String> {
    fs::create_dir_all(root)?;
    let path = token_path(root);
    if let Ok(meta) = fs::metadata(&path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // The home is this writer's own; a token another user owns
            // could have been planted or read.
            let owner = fs::metadata(root)?.uid();
            if meta.uid() != owner {
                anyhow::bail!(
                    "{} belongs to uid {}, not the owner of {} (uid {owner}); remove it and start again",
                    path.display(),
                    meta.uid(),
                    root.display()
                );
            }
            if meta.mode() & 0o077 != 0 {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
        }
        let text = fs::read_to_string(&path)?;
        let token = text.lines().next().unwrap_or("").trim();
        if well_formed(token) {
            return Ok(token.to_string());
        }
    }
    let token = fresh()?;
    let tmp = root.join(format!("{TOKEN_FILE}.tmp"));
    let _ = fs::remove_file(&tmp);
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&tmp)?;
    file.write_all(format!("{token}\n").as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(token)
}

/// Whether a token is the hex this writer writes.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_BYTES * 2 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A new token from the kernel's randomness.
fn fresh() -> anyhow::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether an `Authorization` header carries `token`. The comparison takes
/// the same time wherever the first differing byte is.
#[must_use]
pub fn admits(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header
        .map(str::trim)
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::trim)
    else {
        return false;
    };
    let (a, b) = (given.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_home_gets_a_token_only_its_owner_reads() {
        let dir = tempfile::tempdir().unwrap();
        let token = ensure_token(dir.path()).unwrap();
        assert!(well_formed(&token), "{token}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(token_path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A restart keeps the token, so a client that read it still works.
        assert_eq!(ensure_token(dir.path()).unwrap(), token);
    }

    #[test]
    fn a_token_left_readable_is_closed_and_a_bad_one_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = token_path(dir.path());
        fs::write(&path, "not-a-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let token = ensure_token(dir.path()).unwrap();
        assert!(well_formed(&token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn only_the_exact_bearer_token_is_admitted() {
        let t = "ab".repeat(32);
        assert!(admits(Some(&format!("Bearer {t}")), &t));
        assert!(admits(Some(&format!("  Bearer {t} ")), &t));
        assert!(!admits(None, &t));
        assert!(!admits(Some(&t), &t), "a bare token is not a bearer header");
        assert!(!admits(Some(&format!("Bearer {}", "cd".repeat(32))), &t));
        assert!(!admits(Some("Bearer ab"), &t));
        assert!(!admits(Some("Basic Zm9vOmJhcg=="), &t));
    }
}
