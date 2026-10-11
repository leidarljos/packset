//! `packset migrate-home` moves the old pack home to the XDG one and leaves a
//! link behind; it refuses when a variable names the home.

use std::path::Path;
use std::process::Command;

fn packset(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packset"));
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        // No writer listens here, so the stop check passes.
        .env("PACKSET_PORT", "1");
    cmd
}

#[test]
fn migrate_home_moves_the_old_pack_and_links_it() {
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join(".grokinside/memory");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("token"), "abc\n").unwrap();

    let out = packset(home.path()).arg("migrate-home").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new = home.path().join(".local/share/packset");
    assert_eq!(std::fs::read_to_string(new.join("token")).unwrap(), "abc\n");
    assert!(std::fs::symlink_metadata(&old)
        .unwrap()
        .file_type()
        .is_symlink());

    let again = packset(home.path()).arg("migrate-home").output().unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
}

#[test]
fn migrate_home_leaves_a_named_home_alone() {
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join(".grokinside/memory");
    std::fs::create_dir_all(&old).unwrap();
    let out = packset(home.path())
        .env("PACKSET_HOME", home.path().join("elsewhere"))
        .arg("migrate-home")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stays where it is"));
    assert!(old.is_dir());
    assert!(!home.path().join(".local/share/packset").exists());
}

#[test]
fn migrate_home_refuses_while_a_writer_holds_the_old_home() {
    use std::os::fd::AsRawFd;
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join(".grokinside/memory");
    std::fs::create_dir_all(&old).unwrap();
    let lock = std::fs::File::create(old.join("packsetd.lock")).unwrap();
    // A writer on some other port: it holds the lock, not PACKSET_PORT.
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let out = packset(home.path()).arg("migrate-home").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stop it first"));
    assert!(old.is_dir() && !old.is_symlink());
    drop(lock);
    let out = packset(home.path()).arg("migrate-home").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
