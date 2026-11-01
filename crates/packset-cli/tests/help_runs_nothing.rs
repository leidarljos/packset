//! `packset VERB --help` prints help and runs nothing, against a real writer.
//!
//! Each test starts its own `packsetd` on a free loopback port over a
//! temporary home, so no test touches a seat's writer. A workspace called
//! `--help` is seeded first: before the parser, `packset forget --help`
//! forgot it and `packset stop --help` stopped the writer.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A private writer, stopped when the test ends.
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env_remove("PACKSET_EMBED")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if ureq::get(&format!("{url}/health")).call().is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("packsetd did not answer on {url}");
    }
    Writer { child, url, home }
}

/// `packset` aimed at `writer` and nothing else: its port, its URL, a home
/// with no seat env file.
fn packset(writer: &Writer, args: &[&str]) -> Output {
    let port = writer.url.rsplit(':').next().unwrap();
    Command::new(env!("CARGO_BIN_EXE_packset"))
        .args(args)
        .env("HOME", writer.home.path())
        .env("PACKSET_URL", &writer.url)
        .env("PACKSET_PORT", port)
        .env("PACKSET_HOME", writer.home.path().join("pack"))
        .env("PACKSET_WORKSPACE", "help-test")
        .env_remove("INSIDE_MEMORY_URL")
        .current_dir(Path::new(writer.home.path()))
        .output()
        .unwrap()
}

/// `Authorization: Bearer TOKEN` for the writer's own token.
fn bearer(writer: &Writer) -> String {
    let token = std::fs::read_to_string(writer.home.path().join("pack/token")).unwrap();
    format!("Bearer {}", token.trim())
}

fn seed(writer: &Writer, workspace: &str, text: &str) {
    ureq::post(&format!("{}/v1/atoms", writer.url))
        .set("Authorization", &bearer(writer))
        .send_json(ureq::json!({
            "workspace": workspace,
            "kind": "conclusion",
            "text": text,
        }))
        .unwrap();
}

fn live(writer: &Writer, workspace: &str) -> usize {
    let body: serde_json::Value = ureq::get(&format!("{}/v1/atoms", writer.url))
        .set("Authorization", &bearer(writer))
        .query("workspace", workspace)
        .query("embedding", "omit")
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    body["atoms"].as_array().map_or(0, Vec::len)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn forget_help_forgets_nothing() {
    let writer = writer();
    seed(
        &writer,
        "--help",
        "A claim in a workspace named like a flag.",
    );
    assert_eq!(live(&writer, "--help"), 1);

    for help in ["--help", "-h"] {
        let out = packset(&writer, &["forget", help]);
        assert!(out.status.success(), "{}", text(&out));
        let said = text(&out);
        assert!(said.contains("usage: packset forget WS"), "{said}");
        assert!(!said.contains("forgotten"), "{said}");
    }
    assert_eq!(live(&writer, "--help"), 1, "forget --help forgot");

    // An unknown flag is refused, and forgets nothing either.
    let out = packset(&writer, &["forget", "--hepl"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("unknown flag --hepl"), "{}", text(&out));
    assert_eq!(live(&writer, "--help"), 1, "forget --hepl forgot");

    // `--` is how a workspace that starts with a dash is named.
    let out = packset(&writer, &["forget", "--", "--help"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("1 forgotten from --help"),
        "{}",
        text(&out)
    );
    assert_eq!(live(&writer, "--help"), 0);
}

#[test]
fn help_on_any_verb_writes_nothing_and_stops_nothing() {
    let writer = writer();
    seed(&writer, "help-test", "The one claim this workspace holds.");
    for verb in [
        "remember", "prefer", "sweep", "pin", "grade", "fire", "stop", "start", "ensure", "status",
        "search", "island", "export", "atoms",
    ] {
        let out = packset(&writer, &[verb, "--help"]);
        assert!(out.status.success(), "{verb}: {}", text(&out));
        let said = text(&out);
        assert!(
            said.starts_with("usage: packset ") && said.contains(verb),
            "{verb}: {said}"
        );
    }
    assert_eq!(live(&writer, "help-test"), 1, "a --help wrote a claim");
    assert_eq!(
        live(&writer, "--help"),
        0,
        "a --help wrote to workspace --help"
    );
    assert!(
        ureq::get(&format!("{}/health", writer.url)).call().is_ok(),
        "stop --help stopped the writer"
    );
}
