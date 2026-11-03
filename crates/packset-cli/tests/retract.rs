//! `packset retract` withdraws one claim from a shell.
//!
//! A writer on `--port 0` over a temporary home. `remember` stores a
//! claim, `retract` names the deed, search no longer lists it, and
//! `atoms --as-of` from before the retraction still does.

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

/// `--port 0` asks the kernel for a port. The writer writes that port
/// beside the database; the socket is already bound when the file appears.
fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let pack = home.path().join("pack");
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", "0", "--no-embed", "--home"])
        .arg(&pack)
        .env("HOME", home.path())
        .env_remove("PACKSET_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("INSIDE_MILLI")
        .env_remove("LJOS_INSIDE_MILLI")
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_PORT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let port_path = pack.join("port");
    let deadline = Instant::now() + Duration::from_secs(20);
    let port = loop {
        if let Ok(text) = std::fs::read_to_string(&port_path) {
            if let Ok(port) = text.trim().parse::<u16>() {
                if port != 0 {
                    break port;
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("packsetd did not write the port it bound");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let url = format!("http://127.0.0.1:{port}");
    let ready = Instant::now() + Duration::from_secs(20);
    while Instant::now() < ready {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

/// `packset` aimed at `writer` and nothing else.
fn packset(writer: &Writer, args: &[&str]) -> Output {
    let port = writer.url.rsplit(':').next().unwrap();
    Command::new(env!("CARGO_BIN_EXE_packset"))
        .args(args)
        .env("HOME", writer.home.path())
        .env("PACKSET_URL", &writer.url)
        .env("PACKSET_PORT", port)
        .env("PACKSET_HOME", writer.home.path().join("pack"))
        .env("PACKSET_WORKSPACE", "seat")
        .env_remove("INSIDE_MEMORY_URL")
        .current_dir(Path::new(writer.home.path()))
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

const CLAIM: &str = "Berth nine holds the night crane.";

#[test]
fn retract_drops_the_claim_from_search_and_keeps_it_as_of_before() {
    let writer = writer();

    let remembered = packset(&writer, &["remember", CLAIM]);
    assert!(remembered.status.success(), "{}", text(&remembered));
    let id = stdout(&remembered)
        .split('\t')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!id.is_empty(), "{}", text(&remembered));

    let listed = packset(&writer, &["atoms"]);
    assert!(listed.status.success(), "{}", text(&listed));
    let before = stdout(&listed)
        .lines()
        .find_map(|line| {
            let atom: serde_json::Value = serde_json::from_str(line).ok()?;
            (atom["id"].as_str() == Some(id.as_str())).then(|| {
                atom["ts"]
                    .as_str()
                    .unwrap_or_else(|| panic!("no ts: {line}"))
                    .to_string()
            })
        })
        .unwrap_or_else(|| panic!("atoms did not list {id}: {}", text(&listed)));

    let found = packset(&writer, &["search", "night", "crane"]);
    assert!(found.status.success(), "{}", text(&found));
    let found_out = stdout(&found);
    assert!(
        found_out.contains(CLAIM) && found_out.contains(&id),
        "search did not list the claim before retract: {}",
        text(&found)
    );

    let retracted = packset(&writer, &["retract", &id, "--why", "deed-test-x"]);
    assert!(retracted.status.success(), "{}", text(&retracted));
    assert_eq!(
        stdout(&retracted).trim(),
        format!("{id}\tretracted\tdeed-test-x")
    );

    let after = packset(&writer, &["search", "night", "crane"]);
    assert!(after.status.success(), "{}", text(&after));
    let after_out = stdout(&after);
    assert!(
        !after_out.contains(CLAIM) && !after_out.contains(&id),
        "search still listed the retracted claim: {}",
        text(&after)
    );

    let dated = packset(&writer, &["atoms", "--as-of", &before]);
    assert!(dated.status.success(), "{}", text(&dated));
    assert!(
        stdout(&dated).contains(CLAIM),
        "atoms --as-of {before} dropped the claim: {}",
        text(&dated)
    );

    let live = packset(&writer, &["atoms"]);
    assert!(live.status.success(), "{}", text(&live));
    assert!(
        !stdout(&live).contains(CLAIM),
        "live atoms still listed the retracted claim: {}",
        text(&live)
    );
}
