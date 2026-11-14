//! A write's warning and a hit's labels, as a seat sees them.
//!
//! `remember` and `prefer` print `warning` and `prose.warning` on stderr and
//! leave stdout as one id line. `search` adds a hit's labels as one trailing
//! column only when the hit has any. On develop a plain claim printed nothing
//! on stderr and a four-column search line; those bytes stay.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Output, Stdio};

mod common;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a private writer may take to answer `/health`, the same bound
/// the other CLI tests use.
const WRITER_WAIT: Duration = Duration::from_secs(20);

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

fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .arg("--no-embed")
        .env("HOME", home.path())
        .env_remove("PACKSET_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + WRITER_WAIT;
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
    Command::new(env!("CARGO_BIN_EXE_packset"))
        .args(args)
        .env("HOME", writer.home.path())
        .env("PACKSET_URL", &writer.url)
        .env("PACKSET_PORT", &writer.port)
        .env("PACKSET_HOME", writer.home.path().join("pack"))
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_TOKEN_FILE")
        .env_remove("PACKSET_WORKSPACE")
        .current_dir(writer.home.path())
        .output()
        .unwrap()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// The one stdout line, without its trailing newline.
fn one_line(stdout: &str) -> &str {
    assert!(stdout.ends_with('\n'), "stdout {stdout:?}");
    assert_eq!(stdout.matches('\n').count(), 1, "stdout {stdout:?}");
    stdout.trim_end_matches('\n')
}

/// `{:.4}` as `search` prints a score: digits, a dot, four digits.
fn is_score(field: &str) -> bool {
    let Some((whole, frac)) = field.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.len() == 4
        && frac.bytes().all(|b| b.is_ascii_digit())
}

const CREDENTIAL: &str = "api_key=sk-not-a-real-key-here is the staging key.";
const CREDENTIAL_WARNING: &str = "packset: warning: stored with flag:credential-shaped (a credential); anyone who reads this pack reads the text\n";

#[test]
fn a_credential_shaped_remember_warns_and_search_shows_the_label() {
    let writer = writer();
    let wrote = packset(&writer, &["remember", "--workspace", "flagged", CREDENTIAL]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    let stdout = stdout_of(&wrote);
    let line = one_line(&stdout);
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields.len(), 3, "{line}");
    assert!(!fields[0].is_empty() && fields[0] != "-", "{line}");
    assert_eq!(fields[1], "lesson");
    assert!(
        fields[2].starts_with("due ") && fields[2].contains('T'),
        "{line}"
    );
    assert!(!stdout.contains("flag:"), "{stdout}");
    assert_eq!(stderr_of(&wrote), CREDENTIAL_WARNING);

    let found = packset(
        &writer,
        &["search", "--workspace", "flagged", "staging", "key"],
    );
    assert!(found.status.success(), "{}", stderr_of(&found));
    assert_eq!(stderr_of(&found), "packset: workspace flagged\n");
    let found_stdout = stdout_of(&found);
    let hit = one_line(&found_stdout);
    let cols: Vec<&str> = hit.split('\t').collect();
    assert_eq!(cols.len(), 5, "{hit}");
    assert!(is_score(cols[0]), "{hit}");
    assert_eq!(cols[1], "lesson");
    assert_eq!(cols[2], fields[0]);
    assert_eq!(cols[3], CREDENTIAL);
    assert_eq!(cols[4], "flag:credential-shaped");
}

#[test]
fn a_credential_shaped_prefer_prints_the_same_warning() {
    let writer = writer();
    let wrote = packset(&writer, &["prefer", "--workspace", "flagged", CREDENTIAL]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    let wrote_stdout = stdout_of(&wrote);
    let line = one_line(&wrote_stdout);
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields.len(), 3, "{line}");
    assert_eq!(fields[1], "preference");
    assert!(fields[2].starts_with("due "), "{line}");
    assert_eq!(stderr_of(&wrote), CREDENTIAL_WARNING);
}

#[test]
fn a_plain_remember_and_search_keep_the_unlabelled_lines() {
    let writer = writer();
    let text = "The staging deploy uses the blue channel.";
    let wrote = packset(&writer, &["remember", "--workspace", "plain", text]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    assert_eq!(stderr_of(&wrote), "");
    let wrote_stdout = stdout_of(&wrote);
    let line = one_line(&wrote_stdout);
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields.len(), 3, "{line}");
    assert_eq!(fields[1], "lesson");
    assert!(
        fields[2].starts_with("due ") && fields[2].contains('T'),
        "{line}"
    );

    let found = packset(
        &writer,
        &["search", "--workspace", "plain", "blue", "channel"],
    );
    assert!(found.status.success(), "{}", stderr_of(&found));
    assert_eq!(stderr_of(&found), "packset: workspace plain\n");
    let found_stdout = stdout_of(&found);
    let hit = one_line(&found_stdout);
    let cols: Vec<&str> = hit.split('\t').collect();
    assert_eq!(cols.len(), 4, "{hit}");
    assert!(is_score(cols[0]), "{hit}");
    assert_eq!(cols[1], "lesson");
    assert_eq!(cols[2], fields[0]);
    assert_eq!(cols[3], text);
}

#[test]
fn a_long_remember_prints_the_prose_warning() {
    let writer = writer();
    let text = "Payment webhooks retry three times. The backoff starts at 2 seconds. It doubles each time.";
    let wrote = packset(&writer, &["remember", "--workspace", "prose", text]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    let wrote_stdout = stdout_of(&wrote);
    let _ = one_line(&wrote_stdout);
    assert_eq!(
        stderr_of(&wrote),
        "packset: warning: atom has 3 sentences; one claim is at most 2\n"
    );
}

#[test]
fn remember_help_says_one_lesson() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_packset"))
        .args(["remember", "--help"])
        .env("HOME", home.path())
        .env_remove("PACKSET_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("remember [--workspace WS] TEXT   one lesson\n"),
        "{stdout}"
    );
}

/// `packset-mcp` on the writer's port. Stdout is its JSON-RPC stream.
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_lines<R: std::io::Read + Send + 'static>(read: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn mcp(writer: &Writer) -> Mcp {
    let mut child = Command::new(env!("CARGO_BIN_EXE_packset-mcp"))
        .env("HOME", writer.home.path())
        .env("PACKSET_PORT", &writer.port)
        .env("PACKSET_HOME", writer.home.path().join("pack"))
        .env("PACKSET_WORKSPACE", "mcp")
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_TOKEN_FILE")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // Stderr is drained so a full pipe cannot stall the server.
    let _stderr = spawn_lines(child.stderr.take().unwrap());
    Mcp {
        child,
        stdin,
        lines: spawn_lines(stdout),
    }
}

fn send(mcp: &mut Mcp, message: &Value) {
    writeln!(mcp.stdin, "{message}").unwrap();
    mcp.stdin.flush().unwrap();
}

fn response(mcp: &mut Mcp, id: u64) -> Value {
    let deadline = Instant::now() + WRITER_WAIT;
    while Instant::now() < deadline {
        match mcp.lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    return value;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = mcp.child.try_wait().unwrap() {
                    panic!("packset-mcp exited {status}");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("packset-mcp closed stdout"),
        }
    }
    panic!("no response {id}");
}

#[test]
fn an_mcp_search_row_carries_the_labels() {
    let writer = writer();
    let plain = "The staging deploy uses the blue channel.";
    let wrote = packset(&writer, &["remember", "--workspace", "mcp", CREDENTIAL]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    let wrote = packset(&writer, &["remember", "--workspace", "mcp", plain]);
    assert!(wrote.status.success(), "{}", stderr_of(&wrote));

    let mut mcp = mcp(&writer);
    send(
        &mut mcp,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "packset-test", "version": "0"}
            }
        }),
    );
    let init = response(&mut mcp, 1);
    assert!(init.get("result").is_some(), "{init}");
    send(
        &mut mcp,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut mcp,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "packset_search",
                "arguments": {"query": "staging", "workspace": "mcp"}
            }
        }),
    );
    let called = response(&mut mcp, 2);
    assert!(called.get("error").is_none(), "{called}");
    let rows = called["result"]["structuredContent"]
        .as_array()
        .unwrap_or_else(|| panic!("{called}"));
    let flagged = rows
        .iter()
        .find(|row| row["text"] == CREDENTIAL)
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(
        flagged["labels"],
        serde_json::json!(["flag:credential-shaped"])
    );
    assert_eq!(flagged["kind"], "lesson");
    let plain_row = rows
        .iter()
        .find(|row| row["text"] == plain)
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(plain_row["labels"], serde_json::json!([]));
}
