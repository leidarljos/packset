//! `packset pin NAME` stores that set.
//!
//! The client posts `name`. The route used to read only `set` and clear the
//! pin. The pages quote the object a read returns after that write.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const PAGES: &[&str] = &["docs/source/howto.rst", "docs/orgmode/howto.org"];
const LINE: &str = r#"{"workspace":"seat","set":"review","instructions":""}"#;

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
        .env("PACKSET_WORKSPACE", "seat")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_TOKEN_FILE")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("LJOS_SEAT");
    cmd.output().unwrap()
}

#[test]
fn pin_review_stores_the_set_and_prints_the_line_the_pages_quote() {
    let writer = writer();
    let set = packset(&writer, &["pin", "review"]);
    let stderr = String::from_utf8_lossy(&set.stderr);
    assert!(set.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&set.stdout);
    assert_eq!(stdout.trim(), LINE, "{stdout}");
    let read = packset(&writer, &["pin"]);
    let again = String::from_utf8_lossy(&read.stdout);
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(again.trim(), LINE, "{again}");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in PAGES {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(
            text.lines().filter(|line| line.contains(LINE)).count() >= 2,
            "{path} does not quote the pin twice"
        );
    }
}
