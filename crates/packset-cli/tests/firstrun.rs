//! What a first run is told when a documented step cannot finish.
//!
//! Each case is one failure from a clean home: a missing binary, a taken
//! port, another pack on that port, a different build, a search that
//! matched nothing, a review that is not due yet, and a writer that is down.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

fn packset(home: &Path, port: u16) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packset"));
    cmd.env("HOME", home)
        .env("PACKSET_HOME", home.join("pack"))
        .env("PACKSET_PORT", port.to_string())
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("PACKSET_TIMEOUT_MS", "2000")
        .env_remove("PACKSET_URL")
        .env_remove("INSIDE_MEMORY_URL")
        .env_remove("PACKSET_TOKEN")
        .env_remove("PACKSET_WORKSPACE")
        .env_remove("PACKSET_BIN")
        .env_remove("PACKSET_AUTH");
    cmd
}

fn output(cmd: &mut Command) -> (bool, String, String) {
    let out = common::output(cmd.stdout(Stdio::piped()).stderr(Stdio::piped())).unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wait_health(port: u16, child: &mut std::process::Child, log: &Path) {
    // The same budget the other writer tests use. A cold debug build, with
    // several of those tests opening a store at once, does not fit in less.
    let deadline = Instant::now() + Duration::from_secs(20);
    let url = format!("http://127.0.0.1:{port}/health");
    while Instant::now() < deadline {
        if ureq::get(&url).call().is_ok() {
            return;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let logged = std::fs::read_to_string(log).unwrap_or_default();
    panic!("writer did not answer on {port}\n{logged}");
}

/// One short HTTP answer per connection. `path` is the request target.
///
/// The request is read through before the reply: the headers, then exactly
/// the `Content-Length` body. The write side is shut down and the socket is
/// drained until the client closes. Closing with the body still unread
/// resets the connection, and the client reports that reset as a header
/// error instead of the status.
fn serve(handler: impl Fn(&str) -> (u16, &'static str) + Send + 'static) -> u16 {
    let listener = common::bind();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            answer(conn, &handler);
        }
    });
    port
}

/// The port in `PACKSET_PORT=N packset ensure`.
fn suggested_port(stderr: &str) -> u16 {
    let rest = stderr
        .split("PACKSET_PORT=")
        .nth(1)
        .unwrap_or_else(|| panic!("no port command in {stderr}"));
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("port {digits:?} in {stderr}"))
}

fn answer(conn: TcpStream, handler: &impl Fn(&str) -> (u16, &'static str)) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reader = BufReader::new(conn);
    let Some((path, length)) = read_head(&mut reader) else {
        return;
    };
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let (code, text) = handler(&path);
    let reason = if code == 200 { "OK" } else { "No" };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let mut conn = reader.into_inner();
    let _ = conn.write_all(resp.as_bytes());
    let _ = conn.flush();
    let _ = conn.shutdown(Shutdown::Write);
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let mut discard = [0u8; 256];
    loop {
        match conn.read(&mut discard) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// The request target and `Content-Length`, once the blank line has been read.
fn read_head(reader: &mut BufReader<TcpStream>) -> Option<(String, usize)> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut length = 0usize;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        if line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    Some((path, length))
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// An encoder the `packset` binary would see with `PATH` empty and
/// `PACKSET_EMBED` unset: beside that binary, or the checkout paths it checks.
fn packset_sees_embedder() -> bool {
    let exe = Path::new(env!("CARGO_BIN_EXE_packset"));
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("packset-embed"));
    }
    if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
        candidates.push(root.join("bin/packset-embed"));
        candidates.push(root.join("crates/packset-embed/target/release/packset-embed"));
    }
    candidates.iter().any(|path| executable(path))
}

#[test]
fn the_first_ensure_names_the_home_and_says_when_search_is_lexical() {
    let home = tempfile::tempdir().unwrap();
    let pack = home.path().join("pack");
    let empty = home.path().join("empty-path");
    std::fs::create_dir(&empty).unwrap();
    let (port, ok, stdout, stderr) = common::with_port(|port| {
        let (ok, stdout, stderr) = output(
            packset(home.path(), port)
                .env("PATH", &empty)
                .env_remove("PACKSET_EMBED")
                .env_remove("INSIDE_EMBED")
                .arg("ensure"),
        );
        (port, ok, stdout, stderr)
    });
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains(&format!("PACKSET_URL=http://127.0.0.1:{port}")),
        "{stdout}"
    );
    let home_line = format!("packset: home is {}", pack.display());
    assert!(stderr.contains(&home_line), "{stderr}");
    if packset_sees_embedder() {
        assert!(!stderr.contains("no embeddings"), "{stderr}");
    } else {
        assert!(
            stderr.contains(&format!("{home_line}; search is lexical (no embeddings)")),
            "{stderr}"
        );
    }
    let (ok, _, again) = output(packset(home.path(), port).arg("ensure"));
    assert!(ok, "{again}");
    assert!(!again.contains("home is"), "{again}");
    assert!(!again.contains("lexical"), "{again}");
    let _ = output(packset(home.path(), port).arg("stop"));
}

#[test]
fn the_first_ensure_skips_the_lexical_line_when_an_embedder_is_on_path() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    common::write_executable(&bin.join("packset-embed"), "#!/bin/sh\nexit 0\n");
    let pack = home.path().join("pack");
    let (port, ok, stderr) = common::with_port(|port| {
        let (ok, _, stderr) = output(
            packset(home.path(), port)
                .env("PATH", &bin)
                .env_remove("PACKSET_EMBED")
                .env_remove("INSIDE_EMBED")
                .arg("ensure"),
        );
        (port, ok, stderr)
    });
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains(&format!("packset: home is {}", pack.display())),
        "{stderr}"
    );
    assert!(!stderr.contains("lexical"), "{stderr}");
    assert!(!stderr.contains("no embeddings"), "{stderr}");
    let _ = output(packset(home.path(), port).env("PATH", &bin).arg("stop"));
}

#[test]
fn a_clean_home_remembers_and_says_when_nothing_is_due() {
    let home = tempfile::tempdir().unwrap();
    let (port, ok, stdout, stderr) = common::with_port(|port| {
        let (ok, stdout, stderr) = output(packset(home.path(), port).arg("ensure"));
        (port, ok, stdout, stderr)
    });
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains(&format!("PACKSET_URL=http://127.0.0.1:{port}")),
        "{stdout}"
    );

    let (ok, stdout, stderr) = output(packset(home.path(), port).args([
        "remember",
        "--workspace",
        "demo",
        "The lexical default is BM25+.",
    ]));
    assert!(ok, "{stderr}");
    assert!(stdout.contains("lesson"), "{stdout}");

    let (ok, stdout, stderr) = output(packset(home.path(), port).args([
        "search",
        "--workspace",
        "demo",
        "no-such-token-zz",
    ]));
    assert!(ok, "{stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert!(stderr.contains("no claims matched"), "{stderr}");
    assert!(stderr.contains("no-such-token-zz"), "{stderr}");
    assert!(stderr.contains("demo"), "{stderr}");

    let (ok, _stdout, stderr) = output(packset(home.path(), port).args(["due", "demo"]));
    assert!(ok, "{stderr}");
    assert!(stderr.contains("nothing is due in demo"), "{stderr}");
    assert!(stderr.contains("the next review is"), "{stderr}");

    let (ok, stdout, stderr) = output(packset(home.path(), port).args([
        "island",
        "--workspace",
        "demo",
        "no-such-token-zz",
    ]));
    assert!(ok, "{stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("nothing to show in demo"), "{stderr}");
    assert!(stderr.contains("no claim matched"), "{stderr}");
    assert!(stderr.contains("no-such-token-zz"), "{stderr}");

    let (ok, stdout, stderr) = output(packset(home.path(), port).args([
        "island",
        "--workspace",
        "demo",
        "lexical",
        "default",
    ]));
    assert!(ok, "{stderr}");
    assert!(stdout.contains("lexical default"), "{stdout}");
    assert!(stdout.contains("seed"), "{stdout}");
    assert!(!stderr.contains("nothing to show"), "{stderr}");

    let (ok, _, stderr) = output(packset(home.path(), port).arg("stop"));
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = output(packset(home.path(), port).args(["status", "demo"]));
    assert!(!ok, "a stopped writer reported up: {stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
}

#[test]
fn a_taken_port_names_the_holder_and_the_next_command() {
    let home = tempfile::tempdir().unwrap();
    let socket = common::bind();
    let port = socket.local_addr().unwrap().port();
    let (ok, _, stderr) = output(packset(home.path(), port).arg("ensure"));
    assert!(!ok, "{stderr}");
    assert!(stderr.contains(&std::process::id().to_string()), "{stderr}");
    assert!(stderr.contains("PACKSET_PORT"), "{stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
    let (ok, _, stderr) = output(packset(home.path(), port).arg("stop"));
    assert!(!ok, "{stderr}");
    assert!(stderr.contains(&std::process::id().to_string()), "{stderr}");
    drop(socket);
}

#[test]
fn a_writer_for_another_home_names_both_homes_and_the_next_command() {
    let root = tempfile::tempdir().unwrap();
    let theirs = root.path().join("theirs");
    let mine = root.path().join("mine");
    std::fs::create_dir_all(&theirs).unwrap();
    let err_path = root.path().join("packsetd.err");
    let (port, mut child) = common::with_port(|port| {
        let err = std::fs::File::create(&err_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
            .args(["--port", &port.to_string(), "--home"])
            .arg(&theirs)
            .env("PACKSET_AUTH", "off")
            .env("HOME", root.path())
            .env_remove("PACKSET_HOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        wait_health(port, &mut child, &err_path);
        (port, child)
    });
    let (ok, _, stderr) = output(
        packset(root.path(), port)
            .env("PACKSET_HOME", &mine)
            .arg("ensure"),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(!ok, "{stderr}");
    assert!(stderr.contains(&theirs.display().to_string()), "{stderr}");
    assert!(stderr.contains(&mine.display().to_string()), "{stderr}");
    assert!(stderr.contains(&child.id().to_string()), "{stderr}");
    assert!(stderr.contains("PACKSET_PORT="), "{stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
    assert!(
        !stderr.contains("packset stop"),
        "stopping would take the other pack down: {stderr}"
    );
    assert_ne!(suggested_port(&stderr), port, "{stderr}");
}

#[test]
fn a_different_build_names_both_versions_and_the_next_command() {
    let home = tempfile::tempdir().unwrap();
    let port = serve(|path| {
        if path.starts_with("/health") {
            (200, "packsetd ok")
        } else {
            (200, r#"{"version":"0.9.21","commit":"deadbeef","home":""}"#)
        }
    });
    let (ok, _, stderr) = output(packset(home.path(), port).arg("ensure"));
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("0.9.21"), "{stderr}");
    assert!(stderr.contains("deadbeef"), "{stderr}");
    assert!(
        stderr.contains("packset stop && packset ensure"),
        "{stderr}"
    );
}

#[test]
fn a_refused_token_names_the_next_command() {
    let home = tempfile::tempdir().unwrap();
    let port = serve(|path| {
        if path.starts_with("/health") {
            (200, "packsetd ok")
        } else {
            (401, r#"{"error":"packsetd answers its owner only"}"#)
        }
    });
    let (ok, _, stderr) =
        output(packset(home.path(), port).args(["remember", "--workspace", "demo", "One claim."]));
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("401"), "{stderr}");
    assert!(stderr.contains("token"), "{stderr}");
    assert!(stderr.contains("PACKSET_PORT="), "{stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
    assert!(
        !stderr.contains("packset stop"),
        "stopping would take the other pack down: {stderr}"
    );
    assert_ne!(suggested_port(&stderr), port, "{stderr}");
}

#[test]
fn a_missing_packsetd_names_the_packset_package() {
    let home = tempfile::tempdir().unwrap();
    let bin_dir = home.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let copied = bin_dir.join("packset");
    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_packset")), &copied);
    let (ok, _, stderr) = common::with_port(|port| {
        output(
            Command::new(&copied)
                .arg("ensure")
                .env("HOME", home.path())
                .env("PACKSET_HOME", home.path().join("pack"))
                .env("PACKSET_PORT", port.to_string())
                .env("XDG_STATE_HOME", home.path().join("state"))
                .env("PATH", "/usr/bin:/bin")
                .env_remove("PACKSET_URL")
                .env_remove("PACKSET_BIN"),
        )
    });
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("cargo build --release -p packset"),
        "{stderr}"
    );
    assert!(!stderr.contains("packset-daemon"), "{stderr}");
    assert!(!stderr.contains("packset-cli"), "{stderr}");
}

#[test]
fn an_unwritable_log_directory_names_the_path_and_the_next_command() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let (ok, _, stderr) =
        common::with_port(|port| output(packset(home.path(), port).arg("ensure")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(!ok, "{stderr}");
    assert!(stderr.contains(&state.display().to_string()), "{stderr}");
    assert!(
        stderr.contains("PACKSET_LOG") || stderr.contains("XDG_STATE_HOME"),
        "{stderr}"
    );
}

#[test]
fn the_checkout_wrapper_names_the_packset_package() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = std::fs::read_to_string(root.join("bin/packset")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let path = bin.join("packset");
    common::write_executable(&path, script);
    let out = common::output(
        Command::new(&path)
            .arg("ensure")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("cargo build --release -p packset"),
        "{stderr}"
    );
    assert!(!stderr.contains("packset-cli"), "{stderr}");
}

#[test]
fn a_second_home_is_told_a_free_port_and_that_command_leaves_the_first_writer() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    let err_path = root.path().join("packsetd.err");
    let (port, mut child) = common::with_port(|port| {
        let err = std::fs::File::create(&err_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
            .args(["--port", &port.to_string(), "--home"])
            .arg(&first)
            .env("HOME", root.path())
            .env_remove("PACKSET_HOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        wait_health(port, &mut child, &err_path);
        (port, child)
    });
    let (ok, _, stderr) = output(
        packset(root.path(), port)
            .env("PACKSET_HOME", &second)
            .arg("ensure"),
    );
    assert!(!ok, "{stderr}");
    assert!(stderr.contains(&child.id().to_string()), "{stderr}");
    assert!(stderr.contains(&second.display().to_string()), "{stderr}");
    assert!(stderr.contains("401"), "{stderr}");
    assert!(
        !stderr.contains("packset stop"),
        "stopping would take the other pack down: {stderr}"
    );
    let free = suggested_port(&stderr);
    assert_ne!(free, port, "{stderr}");
    let (ok, stdout, err) = output(
        packset(root.path(), free)
            .env("PACKSET_HOME", &second)
            .arg("ensure"),
    );
    let health = ureq::get(&format!("http://127.0.0.1:{port}/health")).call();
    let _ = output(
        packset(root.path(), free)
            .env("PACKSET_HOME", &second)
            .arg("stop"),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(ok, "{err}\nfrom: {stderr}");
    assert!(stdout.contains(&format!("127.0.0.1:{free}")), "{stdout}");
    assert!(health.is_ok(), "the first writer was stopped");
}

#[test]
fn the_daemon_names_the_process_on_a_taken_port() {
    let home = tempfile::tempdir().unwrap();
    let socket = common::bind();
    let port = socket.local_addr().unwrap().port();
    let out = common::output(
        Command::new(env!("CARGO_BIN_EXE_packsetd"))
            .args(["--port", &port.to_string(), "--home"])
            .arg(home.path())
            .env("HOME", home.path())
            .env_remove("PACKSET_HOME")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    drop(socket);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(&std::process::id().to_string()), "{stderr}");
    assert!(stderr.contains("PACKSET_PORT="), "{stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
    assert!(!stderr.contains("os error"), "{stderr}");
    assert_ne!(suggested_port(&stderr), port, "{stderr}");
}

#[test]
fn a_second_port_on_an_open_home_names_a_new_home() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("pack");
    std::fs::create_dir_all(&home).unwrap();
    let err_path = root.path().join("err");
    let (port, mut child) = common::with_port(|port| {
        let err = std::fs::File::create(&err_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
            .args(["--port", &port.to_string(), "--home"])
            .arg(&home)
            .env("HOME", root.path())
            .env_remove("PACKSET_HOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        wait_health(port, &mut child, &err_path);
        (port, child)
    });
    let other = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_ne!(other, port);
    let started = Instant::now();
    let (ok, _, stderr) = output(
        packset(root.path(), other)
            .env("PACKSET_HOME", &home)
            .arg("ensure"),
    );
    let elapsed = started.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("already open"), "{stderr}");
    assert!(stderr.contains(&child.id().to_string()), "{stderr}");
    assert!(stderr.contains("PACKSET_HOME"), "{stderr}");
    assert!(stderr.contains("PACKSET_PORT="), "{stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
    assert!(
        elapsed < Duration::from_secs(4),
        "ensure waited out startup with no extra news: {elapsed:?}\n{stderr}"
    );
}

/// Long enough that a slow start still passes, and short of the client
/// budget a refused connection used to spend.
const QUICK: Duration = Duration::from_secs(5);

#[test]
fn stop_with_nothing_listening_returns_at_once_and_starts_nothing() {
    let home = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let started = Instant::now();
    let (ok, stdout, stderr) = output(
        packset(home.path(), port)
            .env("PACKSET_TIMEOUT_MS", "30000")
            .arg("stop"),
    );
    let elapsed = started.elapsed();
    assert!(ok, "{stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert!(
        stderr.contains(&format!("nothing of ours to stop on {port}")),
        "{stderr}"
    );
    assert!(!stderr.contains("stopped"), "{stderr}");
    assert!(
        elapsed < QUICK,
        "stop waited on a closed port: {elapsed:?} {stderr}"
    );
    let pack = home.path().join("pack");
    assert!(
        !pack.join("token").exists(),
        "stop wrote a token in {pack:?}"
    );
    assert!(!pack.join("port").exists(), "stop wrote a port file");
    assert!(!pack.join("memory.lmdb").exists(), "stop opened a store");
}

#[test]
fn search_with_nothing_listening_names_ensure_at_once() {
    let home = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let started = Instant::now();
    let (ok, _, stderr) = output(
        packset(home.path(), port)
            .env("PACKSET_TIMEOUT_MS", "30000")
            .args(["search", "--workspace", "demo", "ballots"]),
    );
    let elapsed = started.elapsed();
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "writer is not running on 127.0.0.1:{port}; packset ensure starts it"
        )),
        "{stderr}"
    );
    assert!(
        elapsed < QUICK,
        "search waited on a closed port: {elapsed:?} {stderr}"
    );
    assert!(
        !home.path().join("pack").join("token").exists(),
        "search started a writer"
    );
}
