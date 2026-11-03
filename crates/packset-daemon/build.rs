//! The commit this binary was built from.
//!
//! The version string is the product's, and it does not move between releases
//! while the code does. A seat comparing an installed binary against a
//! repository by version alone reads ten commits of drift as "current", so the
//! binary says which commit it is rather than only which release.
//!
//! `PACKSET_COMMIT` in the environment wins over asking git. A build whose
//! source was copied to a build host without its history would otherwise read
//! whatever commit that host's checkout happens to sit at, and stamp the
//! binary with a commit it was not built from: the same lie, told with more
//! confidence.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PACKSET_COMMIT");
    let root = repository();
    if let Some(root) = root.as_deref() {
        // Cargo reruns this only when a printed path changes. A linked
        // worktree's `.git` is a file (`gitdir: ...`), so `<root>/.git/HEAD`
        // is not a path and the first commit computed there stayed in the
        // binary. `--git-path` names this checkout's own files.
        watch(root);
    }
    let named = std::env::var("PACKSET_COMMIT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let commit = named.unwrap_or_else(|| commit(root.as_deref()));
    println!("cargo:rustc-env=PACKSET_COMMIT={commit}");
}

/// Rebuild when this checkout's commit or index moves.
fn watch(root: &Path) {
    for spec in ["HEAD", "index"] {
        if let Some(path) = git_path(root, spec) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    // On a branch, `HEAD` stays `ref: refs/heads/<name>` across commits.
    // The ref is the file that moves. An empty commit can leave the index
    // unchanged, so the index alone does not mark a new commit.
    if let Some(spec) = git_output(root, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(root, &spec) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// The path git uses for `spec` in this checkout, absolute.
fn git_path(root: &Path, spec: &str) -> Option<PathBuf> {
    let text = git_output(root, &["rev-parse", "--git-path", spec])?;
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(&text);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.exists().then_some(path)
}

/// The workspace root, two directories above this crate.
fn repository() -> Option<PathBuf> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")?;
    Path::new(&manifest)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `<short sha>`, with a trailing `+` when the tree it was built from carried
/// changes that are in no commit. A build from a dirty tree reporting a clean
/// commit is the same lie as a version that never moves.
fn commit(root: Option<&Path>) -> String {
    let Some(root) = root else {
        return "unknown".to_string();
    };
    let Some(sha) =
        git_output(root, &["rev-parse", "--short=8", "HEAD"]).filter(|sha| !sha.is_empty())
    else {
        return "unknown".to_string();
    };
    match git_output(root, &["status", "--porcelain"]) {
        Some(changes) if !changes.is_empty() => format!("{sha}+"),
        _ => sha,
    }
}
