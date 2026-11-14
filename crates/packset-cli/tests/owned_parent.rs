//! An owned writer exits when the process that started it dies.
//!
//! `packset ensure` without `--owned` still detaches: the writer keeps the
//! port after that process has returned. `--owned` holds a pipe the writer
//! reads to EOF, so a signal the owner cannot catch (here SIGKILL) closes
//! the pipe and the writer flushes and leaves. The store is opened again
//! afterwards and the claim is still there.

use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};

mod common;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a cold writer may take to answer `/health`. The other writer
/// tests in this crate wait this long: `packset start` itself gives up at
/// five seconds, and the first run after a build spends the rest paging
/// the binary in.
const CAME_UP: Duration = Duration::from_secs(20);

/// A connect that has not finished in this long is not a live writer. The
/// same bound `procfs` uses before it calls a port free.
const PROBE: Duration = Duration::from_millis(300);

/// How often this test looks at the port, and how soon the owned writer
/// must have left it. `packset start` sleeps this long between probes.
///
/// Parent death closes the pipe in the kernel, and the writer is blocked
/// in `read`, so the next turn flushes and exits. Measured on Intel Xeon
/// (family 6, model 207, 8 cores, KVM) at parent 947c498, debug
/// `cargo test`, polling every 1 ms: 20 runs, max 2 ms from SIGKILL of
/// the parent to a refused connect. One look is twenty-five times that
/// max, so a slower run still finishes inside it.
const LOOK: Duration = Duration::from_millis(50);

const CLAIM: &str = "The owned writer kept this.";

struct Owned {
    child: Child,
    port: u16,
    home: tempfile::TempDir,
}

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = packset(self.home.path(), self.port).arg("stop").output();
    }
}

struct Reader {
    child: Child,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    common::free_port()
}

fn packset(home: &Path, port: u16) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packset"));
    cmd.current_dir(home)
        .env("HOME", home)
        .env("PACKSET_PORT", port.to_string())
        .env("PACKSET_HOME", home.join("pack"))
        .env("PACKSET_AUTH", "off")
        .env("PACKSET_LOG", home.join("packsetd.log"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("LJOS_HOME")
        .env_remove("LJOS_INSIDE_MEMORY_HOME")
        .env_remove("LJOS_MEM_PORT")
        .env_remove("PACKSET_BIN")
        .env_remove("PACKSET_EMBED")
        .env_remove("INSIDE_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("INSIDE_MILLI")
        .env_remove("LJOS_INSIDE_MILLI")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_TOKEN_FILE")
        .stdin(Stdio::null());
    cmd
}

fn answers(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE).is_ok()
}

fn health(port: u16) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/health"))
        .call()
        .ok()
        .and_then(|response| response.into_string().ok())
        .is_some_and(|body| body.starts_with("packsetd"))
}

fn read_banner(stdout: impl Read + Send + 'static) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut banner = String::new();
        let mut lines = BufReader::new(stdout).lines();
        for _ in 0..2 {
            match lines.next() {
                Some(Ok(line)) => {
                    banner.push_str(&line);
                    banner.push('\n');
                }
                _ => break,
            }
        }
        let _ = tx.send(banner);
    });
    rx.recv_timeout(CAME_UP)
        .unwrap_or_else(|_| String::from("timed out waiting for the URL"))
}

fn log_text(home: &Path) -> String {
    std::fs::read_to_string(home.join("packsetd.log")).unwrap_or_default()
}

fn post_claim(port: u16) {
    let url = format!("http://127.0.0.1:{port}/v1/atoms");
    match ureq::post(&url).send_json(ureq::json!({
        "workspace": "owned",
        "kind": "lesson",
        "text": CLAIM,
    })) {
        Ok(response) => {
            response.into_string().expect("write body");
        }
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            panic!("write returned {code}: {body}");
        }
        Err(err) => panic!("write: {err}"),
    }
}

fn claim_texts(port: u16) -> Vec<String> {
    let response = ureq::get(&format!("http://127.0.0.1:{port}/v1/atoms"))
        .query("workspace", "owned")
        .call()
        .unwrap_or_else(|err| panic!("list: {err}"));
    let body: serde_json::Value = response.into_json().expect("list json");
    body["atoms"]
        .as_array()
        .map(|atoms| {
            atoms
                .iter()
                .filter_map(|atom| {
                    atom.get("text")
                        .and_then(|text| text.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn open_reader(home: &Path, port: u16) -> Reader {
    let log = std::fs::File::create(home.join("reader.log")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(home.join("pack"))
        .current_dir(home)
        .env("HOME", home)
        .env("PACKSET_HOME", home.join("pack"))
        .env("PACKSET_AUTH", "off")
        .env_remove("PACKSET_EMBED")
        .env_remove("INSIDE_EMBED")
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_MILLI")
        .env_remove("LJOS_HOME")
        .env_remove("LJOS_INSIDE_MEMORY_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + CAME_UP;
    while Instant::now() < deadline {
        if health(port) {
            return Reader { child };
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(LOOK);
    }
    let _ = child.kill();
    let _ = child.wait();
    let logged = std::fs::read_to_string(home.join("reader.log")).unwrap_or_default();
    panic!("the store did not open again: {logged}");
}

#[test]
fn help_names_the_owned_switch() {
    let home = tempfile::tempdir().unwrap();
    let out = packset(home.path(), 9)
        .args(["ensure", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ensure [--owned]"),
        "ensure help did not name --owned: {text}"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .arg("--help")
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("--owned"),
        "packsetd help did not name --owned: {text}"
    );
}

#[test]
fn a_shared_writer_stays_up_after_ensure_returns() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let out = packset(home.path(), port).arg("ensure").output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}\n{}", log_text(home.path()));
    assert!(
        said.contains(&format!("PACKSET_URL=http://127.0.0.1:{port}")),
        "{said}"
    );
    assert!(
        health(port),
        "ensure returned and the writer was already gone\n{}",
        log_text(home.path())
    );
    assert!(
        !log_text(home.path()).contains("the parent exited"),
        "a shared writer armed the parent watch\n{}",
        log_text(home.path())
    );
    let stopped = packset(home.path(), port).arg("stop").output().unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        !answers(port),
        "stop left the port open: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

#[test]
fn an_owned_writer_exits_when_its_parent_is_killed() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let err_path = home.path().join("ensure.err");
    let err = std::fs::File::create(&err_path).unwrap();
    let mut child = packset(home.path(), port)
        .args(["ensure", "--owned"])
        .stdout(Stdio::piped())
        .stderr(Stdio::from(err))
        .spawn()
        .unwrap();
    let banner = read_banner(child.stdout.take().unwrap());
    if !banner.contains(&format!("PACKSET_URL=http://127.0.0.1:{port}")) {
        let status = child.try_wait().ok().flatten();
        let err = std::fs::read_to_string(&err_path).unwrap_or_default();
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "banner {banner:?} status {status:?}\n{err}\n{}",
            log_text(home.path())
        );
    }
    let mut owned = Owned { child, port, home };
    assert!(
        owned.child.try_wait().unwrap().is_none(),
        "ensure --owned returned while its writer was still up"
    );
    assert!(
        health(port),
        "owned writer did not answer\n{}",
        log_text(owned.home.path())
    );
    post_claim(port);
    assert_eq!(claim_texts(port), vec![CLAIM.to_string()]);

    let killed = Instant::now();
    owned.child.kill().unwrap();
    let _ = owned.child.wait();
    // One look, then the port must already be refusing. The writer wakes
    // on the pipe close; it does not wait for the next look.
    std::thread::sleep(LOOK);
    assert!(
        !answers(port),
        "owned writer still listening {} ms after its parent was killed\n{}",
        killed.elapsed().as_millis(),
        log_text(owned.home.path())
    );
    let logged = log_text(owned.home.path());
    assert!(
        logged.contains("the parent exited; flushed and exiting"),
        "writer left the port without flushing: {logged}"
    );
    assert!(
        !logged.contains("was not armed") && !logged.contains("could not block SIGTERM"),
        "parent-death signal was not armed: {logged}"
    );

    let reader = open_reader(owned.home.path(), port);
    assert_eq!(claim_texts(port), vec![CLAIM.to_string()]);
    drop(reader);
}
