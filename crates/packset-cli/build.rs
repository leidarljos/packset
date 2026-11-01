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
    // A checkout that moves, or a working tree that changes, rebuilds this.
    for signal in ["HEAD", "index"] {
        if let Some(path) = root.as_deref().map(|dir| dir.join(".git").join(signal)) {
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    let named = std::env::var("PACKSET_COMMIT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let commit = named.unwrap_or_else(|| commit(root.as_deref()));
    println!("cargo:rustc-env=PACKSET_COMMIT={commit}");
}

/// The workspace root, two directories above this crate.
fn repository() -> Option<PathBuf> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")?;
    Path::new(&manifest)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// `<short sha>`, with a trailing `+` when the tree it was built from carried
/// changes that are in no commit. A build from a dirty tree reporting a clean
/// commit is the same lie as a version that never moves.
fn commit(root: Option<&Path>) -> String {
    let Some(root) = root else {
        return "unknown".to_string();
    };
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let Some(sha) = run(&["rev-parse", "--short=8", "HEAD"]).filter(|s| !s.is_empty()) else {
        return "unknown".to_string();
    };
    match run(&["status", "--porcelain"]) {
        Some(changes) if !changes.is_empty() => format!("{sha}+"),
        _ => sha,
    }
}
