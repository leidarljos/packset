//! The query encoder starts when `packsetd` binds, not on the first search.
//!
//! A stub stands in for `packset-embed`, so the test does not need the
//! ONNX runtime and runs on macOS the same way. No search is issued.

use std::path::{Path, PathBuf};

mod common;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Writer {
    child: Child,
    url: String,
    _home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    common::free_port()
}

fn program_on_path(name: &str) -> PathBuf {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is not on PATH"))
}

fn spawn(home: &Path, embed: &Path, inside: &str, cache: &Path) -> Writer {
    let port = free_port();
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(home)
        .env("HOME", root.path())
        .env("PACKSET_AUTH", "off")
        .env("INSIDE_EMBED", inside)
        .env("PACKSET_EMBED", embed)
        .env("PACKSET_EMBED_CACHE", cache)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .env_remove("PACKSET_PORT")
        .env_remove("PACKSET_MILLI")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("LJOS_SEAT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer {
                child,
                url,
                _home: root,
            };
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn status(url: &str) -> Value {
    ureq::get(&format!("{url}/v1/status"))
        .call()
        .unwrap_or_else(|err| panic!("status: {err}"))
        .into_json()
        .unwrap()
}

/// The writer is listening, and the encoder is already loading, before any
/// search. `remaining_ms` counts down from the recorded cold start, then
/// disappears once the encoder answers.
#[test]
fn the_encoder_warms_when_the_writer_starts_listening() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("asked");
    let stub = dir.path().join("stub-embed");
    common::write_executable(
        &stub,
        format!(
            r#"#!/usr/bin/env python3
import json, sys, time, pathlib
log = pathlib.Path(r"{log}")
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    asked = json.loads(line)
    text = asked.get("text") or ""
    with log.open("a") as handle:
        handle.write(text + "\n")
    time.sleep(1.5)
    print(json.dumps({{"v": [0.1, 0.2]}}), flush=True)
"#,
            log = log.display()
        ),
    );
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    // A recorded cold start, so the countdown is this number and not the
    // unmeasured ceiling.
    std::fs::write(cache.join("load-ms"), "5000\n").unwrap();
    let pack = dir.path().join("pack");
    let started = Instant::now();
    let writer = spawn(&pack, &stub, "on", &cache);
    let health_ms = started.elapsed();
    let health = ureq::get(&format!("{}/health", writer.url))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(health, "packsetd ok");
    assert!(
        health_ms < Duration::from_millis(1500),
        "health waited on the encoder: {health_ms:?}"
    );

    let mut saw = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let done = loop {
        assert!(
            Instant::now() < deadline,
            "the encoder did not answer; asked {:?}",
            std::fs::read_to_string(&log).ok()
        );
        let body = status(&writer.url);
        let answering = body["embedder"]["answering"].clone();
        let remaining = body["embedder"].get("remaining_ms").and_then(Value::as_u64);
        if answering.is_null() {
            // The warm thread may not have stamped the load on the first
            // poll. A later poll during the stub's sleep must.
            if let Some(ms) = remaining {
                assert!(ms <= 5_000, "{ms}");
                if saw.is_none() {
                    saw = Some(ms);
                }
            }
        } else if answering == Value::Bool(true) {
            assert!(
                remaining.is_none(),
                "remaining_ms stayed after the encoder answered: {body}"
            );
            break body;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let ms = saw.expect("status never showed a load in progress");
    assert!(
        ms > 1_000,
        "remaining_ms {ms} did not use the recorded 5000 ms cold start"
    );
    let asked = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        asked.lines().collect::<Vec<_>>(),
        vec!["."],
        "a search reached the encoder: {asked:?}"
    );
    assert!(done["embedder"]["answering"].as_bool().unwrap());
}

/// Switched off, or a binary that exits, is not a load in progress.
/// `remaining_ms` is absent in both.
#[test]
fn remaining_ms_is_absent_when_the_encoder_is_off_or_down() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("started");
    let stub = dir.path().join("stub-embed");
    common::write_executable(
        &stub,
        format!(
            "#!/bin/sh\nprintf started > '{}'\nexit 1\n",
            sentinel.display()
        ),
    );
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let off = spawn(&dir.path().join("off"), &stub, "off", &cache);
    std::thread::sleep(Duration::from_millis(200));
    let body = status(&off.url);
    assert_eq!(body["embedder"]["enabled"], Value::Bool(false), "{body}");
    assert_eq!(body["embedder"]["available"], Value::Bool(false), "{body}");
    assert!(body["embedder"]["answering"].is_null(), "{body}");
    assert!(body["embedder"].get("remaining_ms").is_none(), "{body}");
    assert!(
        !sentinel.exists(),
        "the encoder child was started with the switch off"
    );
    drop(off);

    let down = spawn(
        &dir.path().join("down"),
        &program_on_path("false"),
        "on",
        &cache,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let body = loop {
        assert!(
            Instant::now() < deadline,
            "a binary that exits was still loading"
        );
        let body = status(&down.url);
        if body["embedder"]["answering"] == Value::Bool(false) {
            break body;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(body["embedder"]["available"], Value::Bool(false), "{body}");
    assert!(
        body["embedder"].get("remaining_ms").is_none(),
        "remaining_ms stayed after the encoder failed: {body}"
    );
}
