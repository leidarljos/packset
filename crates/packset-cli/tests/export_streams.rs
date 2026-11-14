//! `packset export` keeps a pipe clean.
//!
//! The atoms go to the satchel. The count goes to stderr. Stdout is the
//! cited accessions, one a line, and nothing else.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

mod common;

const COUNT_ON_STDERR: &str =
    "That count is on stderr. Stdout is the cited accessions, one a line, and";
const REFERENCE: &str = "the count on stderr; cited accessions on stdout";

struct Writer {
    child: Child,
    url: String,
    port: String,
    home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn clear_seat(cmd: &mut Command) {
    for key in [
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "CURSOR_CONVERSATION_ID",
        "LJOS_SEAT",
        "PACKSET_FUSE",
        "PACKSET_DIVERSIFY",
        "PACKSET_DECAY",
        "PACKSET_STEM",
        "PACKSET_MILLI",
        "INSIDE_MILLI",
        "LJOS_INSIDE_MILLI",
        "PACKSET_EMBED",
        "INSIDE_EMBED",
    ] {
        cmd.env_remove(key);
    }
}

fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packsetd"));
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .arg("--no-embed")
        .env("HOME", home.path())
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    clear_seat(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer {
                child,
                url,
                port: port.to_string(),
                home,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn packset(writer: &Writer, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packset"));
    cmd.args(args)
        .env("HOME", writer.home.path())
        .env("PACKSET_URL", &writer.url)
        .env("PACKSET_PORT", &writer.port)
        .env("PACKSET_HOME", writer.home.path().join("pack"))
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_TOKEN_FILE")
        .env_remove("PACKSET_WORKSPACE")
        .current_dir(writer.home.path());
    clear_seat(&mut cmd);
    cmd.output().unwrap()
}

fn post(writer: &Writer, body: serde_json::Value) {
    let token = std::fs::read_to_string(writer.home.path().join("pack/token")).unwrap();
    ureq::post(&format!("{}/v1/atoms", writer.url))
        .set("Authorization", &format!("Bearer {}", token.trim()))
        .send_json(body)
        .unwrap_or_else(|err| panic!("post atom: {err}"));
}

fn texts(file: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(file).unwrap();
    assert!(
        !raw.contains("atoms to"),
        "the satchel carries the count: {raw}"
    );
    raw.lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect(line);
            value["text"].as_str().unwrap_or("").to_string()
        })
        .collect()
}

#[test]
fn cited_accessions_are_stdout_and_the_count_is_stderr() {
    let writer = writer();
    post(
        &writer,
        serde_json::json!({
            "workspace": "demo",
            "kind": "lesson",
            "text": "The overlay landed as a frozen deed.",
            "entities": ["sha256:abc", "deed-patch-overlay"],
        }),
    );
    post(
        &writer,
        serde_json::json!({
            "workspace": "demo",
            "kind": "preference",
            "text": "CombMNZ over RRF for fusing two ballots.",
        }),
    );

    let into = writer.home.path().join("bag");
    let out = packset(
        &writer,
        &["export", "--into", into.to_str().unwrap(), "demo"],
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stderr}");
    let file = into.join("demo.jsonl");
    assert_eq!(
        stderr,
        format!("2 atoms to {}\n", file.display()),
        "the count is the whole of stderr"
    );
    assert_eq!(
        stdout, "deed-patch-overlay\nsha256:abc\n",
        "stdout is the accession list"
    );
    assert!(!stdout.contains("atoms"), "{stdout}");
    assert!(!stderr.contains("deed-"), "{stderr}");
    assert!(!stderr.contains("sha256:"), "{stderr}");
    let mut stored = texts(&file);
    stored.sort();
    assert_eq!(
        stored,
        vec![
            "CombMNZ over RRF for fusing two ballots.".to_string(),
            "The overlay landed as a frozen deed.".to_string(),
        ]
    );
}

#[test]
fn a_pack_that_cites_nothing_leaves_stdout_empty() {
    let writer = writer();
    post(
        &writer,
        serde_json::json!({
            "workspace": "plain",
            "kind": "lesson",
            "text": "The lexical default is BM25+.",
        }),
    );
    let into = writer.home.path().join("bag");
    let out = packset(
        &writer,
        &["export", "--into", into.to_str().unwrap(), "plain"],
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stderr}");
    let file = into.join("plain.jsonl");
    assert_eq!(stdout, "");
    assert_eq!(stderr, format!("1 atoms to {}\n", file.display()));
    assert_eq!(
        texts(&file),
        vec!["The lexical default is BM25+.".to_string()]
    );
}

#[test]
fn the_docs_name_the_two_streams() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in [
        "docs/source/getting-started.rst",
        "docs/orgmode/getting-started.org",
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(text.contains(COUNT_ON_STDERR), "{path}");
    }
    for path in ["docs/source/reference.rst", "docs/orgmode/reference.org"] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(text.contains(REFERENCE), "{path}");
    }
    for path in ["docs/source/howto.rst", "docs/orgmode/howto.org"] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(
            text.contains("The count is on stderr."),
            "{path} shows the pipe without saying where the count goes"
        );
    }

    let out = Command::new(env!("CARGO_BIN_EXE_packset"))
        .args(["export", "--help"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(said.contains(REFERENCE), "{said}");
}
