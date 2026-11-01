//! The writer answers its owner only.
//!
//! A real `packsetd` on a free loopback port over a temporary home. Another
//! local user is a client whose home holds another token, or none.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Writer {
    child: Child,
    url: String,
    home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn writer(auth_off: bool) -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packsetd"));
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env_remove("PACKSET_EMBED")
        .env_remove("PACKSET_AUTH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if auth_off {
        cmd.env("PACKSET_AUTH", "off");
    }
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn status_of(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn only_the_token_in_the_pack_home_opens_the_pack() {
    let w = writer(false);
    let path = w.home.path().join("pack/token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the token is its owner's alone");
    }
    let token = std::fs::read_to_string(&path).unwrap().trim().to_string();
    let atoms = format!("{}/v1/atoms", w.url);

    // The liveness probe stays open: a client finds the writer before it
    // has read the token.
    assert_eq!(
        status_of(ureq::get(&format!("{}/health", w.url)).call()),
        200
    );
    // No token, a wrong one, and a bare one are all refused, reads and writes.
    assert_eq!(
        status_of(ureq::get(&atoms).query("workspace", "w").call()),
        401
    );
    assert_eq!(
        status_of(
            ureq::post(&atoms)
                .set("Authorization", &format!("Bearer {}", "0".repeat(64)))
                .send_json(ureq::json!({"workspace": "w", "kind": "conclusion", "text": "x"}))
        ),
        401
    );
    assert_eq!(
        status_of(
            ureq::get(&format!("{}/v1/status", w.url))
                .set("Authorization", &token)
                .call()
        ),
        401
    );
    // The owner's token reads and writes.
    assert_eq!(
        status_of(
            ureq::post(&atoms)
                .set("Authorization", &format!("Bearer {token}"))
                .send_json(
                    ureq::json!({"workspace": "w", "kind": "conclusion", "text": "A claim."})
                )
        ),
        200
    );
    assert_eq!(
        status_of(
            ureq::get(&atoms)
                .set("Authorization", &format!("Bearer {token}"))
                .query("workspace", "w")
                .call()
        ),
        200
    );
}

#[test]
fn the_cli_reads_the_token_and_a_stranger_is_told_why() {
    let w = writer(false);
    let port = w.url.rsplit(':').next().unwrap().to_string();
    let run = |home: &std::path::Path, pack: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_packset"))
            .args([
                "remember",
                "--workspace",
                "tok",
                "The token gates the pack.",
            ])
            .env("HOME", home)
            .env("PACKSET_URL", &w.url)
            .env("PACKSET_PORT", &port)
            .env("PACKSET_HOME", pack)
            .env_remove("PACKSET_TOKEN")
            .env_remove("PACKSET_TOKEN_FILE")
            .env_remove("INSIDE_MEMORY_URL")
            .output()
            .unwrap()
    };
    let own = run(w.home.path(), &w.home.path().join("pack"));
    assert!(
        own.status.success(),
        "{}",
        String::from_utf8_lossy(&own.stderr)
    );
    let stranger = tempfile::tempdir().unwrap();
    let theirs = run(stranger.path(), &stranger.path().join("pack"));
    assert!(!theirs.status.success());
    let said = String::from_utf8_lossy(&theirs.stderr);
    assert!(said.contains("401") && said.contains("token"), "{said}");
}

#[test]
fn auth_off_admits_a_client_without_a_token() {
    let w = writer(true);
    assert!(!w.home.path().join("pack/token").exists());
    assert_eq!(
        status_of(
            ureq::get(&format!("{}/v1/atoms", w.url))
                .query("workspace", "w")
                .call()
        ),
        200
    );
}
