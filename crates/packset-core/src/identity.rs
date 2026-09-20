//! Workspace naming: same human, same repository, every client.
//!
//! The default workspace is the normalized git remote, so two harnesses in one
//! checkout agree without either being told. Asking git for the remote is I/O
//! and belongs to the caller; everything here is a pure function of the string
//! it is handed.

/// Schema name for an identity record.
pub const SCHEMA: &str = "inside.identity/v1";

/// How a workspace name is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The normalized git remote, falling back to the directory.
    PerRepo,
    /// The resolved working directory.
    PerDirectory,
    /// One named session.
    PerSession,
    /// One name for the seat.
    Global,
}

impl Strategy {
    /// Parse a strategy name from the wire.
    ///
    /// # Errors
    ///
    /// Returns the unrecognised name.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "per-repo" => Ok(Self::PerRepo),
            "per-directory" => Ok(Self::PerDirectory),
            "per-session" => Ok(Self::PerSession),
            "global" => Ok(Self::Global),
            other => Err(format!("unknown workspace_strategy: {other}")),
        }
    }

    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PerRepo => "per-repo",
            Self::PerDirectory => "per-directory",
            Self::PerSession => "per-session",
            Self::Global => "global",
        }
    }
}

/// A git remote as a workspace name.
///
/// `https://host/owner/repo.git`, `git@host:owner/repo.git` and
/// `ssh://git@host/owner/repo` all land on `git:host/owner/repo`, which is what
/// makes two clients in one checkout agree.
///
/// # Errors
///
/// Returns a message when the string is empty or is not a remote shape.
pub fn normalize_remote(url: &str) -> Result<String, String> {
    let raw = url.trim();
    if raw.is_empty() {
        return Err("empty git remote".into());
    }
    for scheme in ["http://", "https://", "ssh://"] {
        if let Some(rest) = raw.strip_prefix(scheme) {
            let rest = rest.split_once('@').map_or(rest, |(_, after)| after);
            let (host, path) = rest.split_once('/').ok_or_else(|| bad(raw))?;
            let host = host.split_once(':').map_or(host, |(h, _)| h);
            return Ok(format!(
                "git:{host}/{}",
                path.trim_start_matches('/').trim_end_matches(".git")
            ));
        }
    }
    // scp-like: [user@]host:path
    let rest = raw.split_once('@').map_or(raw, |(_, after)| after);
    let (host, path) = rest
        .split_once(':')
        .or_else(|| rest.split_once('/'))
        .ok_or_else(|| bad(raw))?;
    if host.is_empty() || path.is_empty() {
        return Err(bad(raw));
    }
    Ok(format!("git:{host}/{}", path.trim_end_matches(".git")))
}

fn bad(raw: &str) -> String {
    format!("unrecognized git remote: {raw}")
}

/// A workspace name as a directory component.
///
/// Every run of anything but a letter, digit, dot, underscore or hyphen becomes
/// one underscore, so `git:github.com/HaoZeke/vissue` names a directory without
/// nesting one.
#[must_use]
pub fn workspace_slug(workspace: &str) -> String {
    let mut out = String::with_capacity(workspace.len());
    let mut pending_underscore = false;
    for ch in workspace.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            pending_underscore = false;
            out.push(ch);
        } else {
            pending_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "workspace".into()
    } else {
        trimmed.to_string()
    }
}

/// The peer name for one harness, with a non-default profile appended.
///
/// # Errors
///
/// Returns a message when the harness name is empty.
pub fn agent_peer(harness: &str, profile: Option<&str>) -> Result<String, String> {
    let name = harness.trim();
    if name.is_empty() {
        return Err("harness is required".into());
    }
    match profile.map(str::trim) {
        Some(p) if !p.is_empty() && p != "default" => Ok(format!("{name}.{p}")),
        _ => Ok(name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_and_ssh_land_on_the_same_workspace() {
        let https = normalize_remote("https://example.com/acme/widgets.git").unwrap();
        let ssh = normalize_remote("git@example.com:acme/widgets.git").unwrap();
        assert_eq!(https, "git:example.com/acme/widgets");
        assert_eq!(ssh, https);
    }

    #[test]
    fn the_ssh_scheme_form_agrees_too() {
        assert_eq!(
            normalize_remote("ssh://git@example.com/acme/widgets.git").unwrap(),
            "git:example.com/acme/widgets"
        );
    }

    #[test]
    fn a_port_is_not_part_of_the_host() {
        assert_eq!(
            normalize_remote("https://example.com:8443/team/repo.git").unwrap(),
            "git:example.com/team/repo"
        );
    }

    #[test]
    fn an_empty_or_shapeless_remote_is_refused() {
        assert!(normalize_remote("").is_err());
        assert!(normalize_remote("   ").is_err());
        assert!(normalize_remote("just-a-word").is_err());
    }

    #[test]
    fn a_slug_is_one_directory_component() {
        assert_eq!(
            workspace_slug("git:example.com/acme/widgets"),
            "git_example.com_acme_widgets"
        );
        assert_eq!(workspace_slug("global"), "global");
        assert_eq!(workspace_slug("///"), "workspace");
        assert_eq!(workspace_slug(""), "workspace");
    }

    #[test]
    fn a_profile_shows_only_when_it_is_not_the_default() {
        assert_eq!(agent_peer("hermes", None).unwrap(), "hermes");
        assert_eq!(agent_peer("hermes", Some("default")).unwrap(), "hermes");
        assert_eq!(agent_peer("hermes", Some("")).unwrap(), "hermes");
        assert_eq!(agent_peer("hermes", Some("work")).unwrap(), "hermes.work");
        assert!(agent_peer("  ", None).is_err());
    }

    #[test]
    fn strategy_names_round_trip() {
        for name in ["per-repo", "per-directory", "per-session", "global"] {
            assert_eq!(Strategy::parse(name).unwrap().as_str(), name);
        }
        assert!(Strategy::parse("per-mood").is_err());
    }
}
