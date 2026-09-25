//! Naming the workspace a client is in.
//!
//! The default is the normalized git remote, so two harnesses in one checkout
//! agree without either being told. Asking git is the only I/O here; the rule
//! it feeds lives in [`packset_core::identity`].

use std::path::Path;
use std::process::Command;

use packset_core::identity::{self, Strategy};
use serde_json::{json, Value};

/// `git remote get-url origin` at `cwd`, or nothing.
#[must_use]
pub fn git_remote(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// The workspace name for a directory under one strategy.
///
/// A checkout with no remote falls back to its own path rather than to a shared
/// name: two unrelated projects sharing `global` by accident is how a pack ends
/// up answering one with the other's memory.
#[must_use]
pub fn resolve(cwd: &Path, strategy: Strategy, session: Option<&str>) -> String {
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    match strategy {
        Strategy::Global => "global".into(),
        Strategy::PerSession => session.map_or_else(
            || format!("dir:{}", root.display()),
            |id| format!("session:{id}"),
        ),
        Strategy::PerDirectory => format!("dir:{}", root.display()),
        Strategy::PerRepo => git_remote(&root)
            .and_then(|url| identity::normalize_remote(&url).ok())
            .unwrap_or_else(|| format!("dir:{}", root.display())),
    }
}

/// The peer name for the person at the seat.
#[must_use]
pub fn user_peer(explicit: Option<&str>) -> String {
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    for var in ["GROK_INSIDE_USER_PEER", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    "unknown".into()
}

/// The identity record for one client.
///
/// # Errors
///
/// Returns a message when the harness name is empty.
pub fn identity(
    cwd: &Path,
    strategy: Strategy,
    harness: &str,
    profile: Option<&str>,
    session: Option<&str>,
    turn: i64,
    user: Option<&str>,
) -> Result<Value, String> {
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Ok(json!({
        "schema": identity::SCHEMA,
        "workspace": resolve(&root, strategy, session),
        "workspace_strategy": strategy.as_str(),
        "user_peer": user_peer(user),
        "agent_peer": identity::agent_peer(harness, profile)?,
        "harness": harness,
        "profile": profile.unwrap_or(""),
        "session_id": session,
        "cwd": root.display().to_string(),
        "turn": turn,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_is_global_wherever_it_is_asked() {
        assert_eq!(resolve(Path::new("/tmp"), Strategy::Global, None), "global");
    }

    #[test]
    fn a_directory_strategy_names_the_directory() {
        let name = resolve(Path::new("/tmp"), Strategy::PerDirectory, None);
        assert!(name.starts_with("dir:"), "{name}");
    }

    #[test]
    fn a_session_without_an_id_falls_back_to_the_directory() {
        // Sharing one name across unrelated sessions would answer one with
        // another's memory, so the fallback is the narrower thing.
        let name = resolve(Path::new("/tmp"), Strategy::PerSession, None);
        assert!(name.starts_with("dir:"), "{name}");
        assert_eq!(
            resolve(Path::new("/tmp"), Strategy::PerSession, Some("abc")),
            "session:abc"
        );
    }

    #[test]
    fn a_checkout_with_no_remote_names_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let name = resolve(dir.path(), Strategy::PerRepo, None);
        assert!(name.starts_with("dir:"), "{name}");
    }

    #[test]
    fn an_identity_carries_the_schema_and_the_peers() {
        let dir = tempfile::tempdir().unwrap();
        let ident = identity(
            dir.path(),
            Strategy::PerRepo,
            "hermes",
            None,
            None,
            0,
            Some("rg"),
        )
        .unwrap();
        assert_eq!(ident["schema"], json!(packset_core::identity::SCHEMA));
        assert_eq!(ident["agent_peer"], json!("hermes"));
        assert_eq!(ident["user_peer"], json!("rg"));
        assert_eq!(ident["workspace_strategy"], json!("per-repo"));
        assert_eq!(ident["turn"], json!(0));
        assert!(identity(dir.path(), Strategy::PerRepo, "", None, None, 0, None).is_err());
    }
}
