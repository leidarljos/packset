//! A fresh `packset search` of the getting-started claims.
//!
//! The pages quote this run. The writer has no encoder, so the ranking is
//! the linear one, and the scores are the ones printed in the same second
//! the claims were written.

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

mod common;

const LESSON: &str = "The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25.";
const PREFERENCE: &str = "CombMNZ over RRF for fusing two ballots.";

const PAGES: &[&str] = &[
    "docs/source/getting-started.rst",
    "docs/orgmode/getting-started.org",
    "docs/source/index.rst",
    "docs/orgmode/index.org",
];

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
        "PACKSET_RERANK",
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

fn is_score(field: &str) -> bool {
    let Some((whole, frac)) = field.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.len() == 4
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// The first `n - 1` fields split on two spaces; the last field keeps the rest.
fn split_fields(line: &str, n: usize) -> Vec<&str> {
    let mut out = Vec::with_capacity(n);
    let mut rest = line;
    for _ in 1..n {
        let Some((head, tail)) = rest.split_once("  ") else {
            out.push(rest);
            return out;
        };
        out.push(head);
        rest = tail;
    }
    out.push(rest);
    out
}

/// Score, kind, text. The id in the third column is this run's.
fn printed(stdout: &str) -> Vec<(String, String, String)> {
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let cols = split_fields(line, 4);
            assert_eq!(cols.len(), 4, "{line}");
            assert!(is_score(cols[0]), "{line}");
            assert_eq!(cols[2].len(), 32, "{line}");
            assert!(cols[2].bytes().all(|b| b.is_ascii_hexdigit()), "{line}");
            (
                cols[0].to_string(),
                cols[1].to_string(),
                cols[3].to_string(),
            )
        })
        .collect()
}

fn quoted(path: &str) -> Vec<(String, String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join(path)).unwrap();
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || !is_score(fields[0]) {
                return None;
            }
            let claim = fields[3..].join(" ");
            if claim != LESSON && claim != PREFERENCE {
                return None;
            }
            Some((fields[0].to_string(), fields[1].to_string(), claim))
        })
        .collect()
}

#[test]
fn a_fresh_search_prints_the_lines_the_pages_quote() {
    let writer = writer();
    let remembered = packset(&writer, &["remember", "--workspace", "demo", LESSON]);
    assert!(
        remembered.status.success(),
        "{}",
        String::from_utf8_lossy(&remembered.stderr)
    );
    let preferred = packset(&writer, &["prefer", "--workspace", "demo", PREFERENCE]);
    assert!(
        preferred.status.success(),
        "{}",
        String::from_utf8_lossy(&preferred.stderr)
    );

    let found = packset(
        &writer,
        &["search", "--workspace", "demo", "ballots", "BM25"],
    );
    let stderr = String::from_utf8_lossy(&found.stderr);
    assert!(found.status.success(), "{stderr}");
    assert_eq!(stderr, "packset: workspace demo\n");
    let hits = printed(&String::from_utf8_lossy(&found.stdout));
    assert_eq!(hits.len(), 2, "{hits:?}");
    for path in PAGES {
        assert_eq!(&quoted(path), &hits, "{path}");
    }
}
