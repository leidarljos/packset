//! `packset search` names the rank and the rerank stage it asked for.
//!
//! The howto quotes those lines, and its curls send the pack token.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const LESSON: &str = "The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25.";
const PREFERENCE: &str = "CombMNZ over RRF for fusing two ballots.";
const PAGES: &[&str] = &["docs/source/howto.rst", "docs/orgmode/howto.org"];

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
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn packset(writer: &Writer, args: &[&str]) -> std::process::Output {
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
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("LJOS_SEAT");
    cmd.output().unwrap()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn search_names_the_rank_and_the_rerank_stage() {
    let writer = writer();
    for (verb, text) in [("remember", LESSON), ("prefer", PREFERENCE)] {
        let wrote = packset(&writer, &[verb, "--workspace", "demo", text]);
        assert!(wrote.status.success(), "{}", stderr_of(&wrote));
    }

    let plain = packset(
        &writer,
        &["search", "--workspace", "demo", "ballots", "BM25"],
    );
    let plain_err = stderr_of(&plain);
    assert!(plain.status.success(), "{plain_err}");
    assert_eq!(plain_err, "packset: workspace demo\n");

    let both = packset(
        &writer,
        &[
            "search",
            "--rerank",
            "--workspace",
            "demo",
            "ballots",
            "BM25",
        ],
    );
    let both_err = stderr_of(&both);
    assert!(both.status.success(), "{both_err}");
    assert!(both_err.contains("packset: rerank: absent"), "{both_err}");
    assert!(!both_err.contains("packset: rank:"), "{both_err}");
    assert!(
        String::from_utf8_lossy(&both.stdout).lines().count() >= 2,
        "{}",
        String::from_utf8_lossy(&both.stdout)
    );

    let one = packset(
        &writer,
        &["search", "--rerank", "--workspace", "demo", "CombMNZ"],
    );
    let one_err = stderr_of(&one);
    assert!(one.status.success(), "{one_err}");
    assert!(one_err.contains("packset: rerank: settled"), "{one_err}");
    assert_eq!(
        String::from_utf8_lossy(&one.stdout).lines().count(),
        1,
        "{}",
        String::from_utf8_lossy(&one.stdout)
    );

    let ranked = packset(
        &writer,
        &[
            "search",
            "--rank",
            "relevance",
            "--workspace",
            "demo",
            "ballots",
        ],
    );
    let ranked_err = stderr_of(&ranked);
    assert!(ranked.status.success(), "{ranked_err}");
    assert!(
        ranked_err.contains("packset: rank: relevance"),
        "{ranked_err}"
    );
    assert!(!ranked_err.contains("packset: rerank:"), "{ranked_err}");
}

#[test]
fn howto_curls_send_the_pack_token() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in PAGES {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let curls: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("curl") && line.contains("PACKSET_URL"))
            .collect();
        assert_eq!(curls.len(), 3, "{path}: {curls:?}");
        for line in curls {
            assert!(line.contains("Authorization: Bearer"), "{path}: {line}");
            assert!(
                line.contains("~/.local/share/packset/token"),
                "{path}: {line}"
            );
            assert!(!line.contains('\t'), "{path}: {line}");
        }
    }
}
