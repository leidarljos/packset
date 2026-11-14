//! What a first run is told when a documented step cannot finish.
//!
//! Each case is one failure from a clean home: a missing binary, a taken
//! port, another pack on that port, a different build, a search that
//! matched nothing, a review that is not due yet, and a writer that is down.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_health(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let url = format!("http://127.0.0.1:{port}/health");
    while Instant::now() < deadline {
        if ureq::get(&url).call().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("writer did not answer on {port}");
}

/// One short HTTP answer per connection. `path` is the request target.
///
/// The request is read through before the reply: the headers, then exactly
/// the `Content-Length` body. The write side is shut down and the socket is
/// drained until the client closes. Closing with the body still unread
/// resets the connection, and the client reports that reset as a header
/// error instead of the status.
fn serve(handler: impl Fn(&str) -> (u16, &'static str) + Send + 'static) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            answer(conn, &handler);
        }
    });
    port
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

#[test]
fn a_clean_home_remembers_and_says_when_nothing_is_due() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let (ok, stdout, stderr) = output(packset(home.path(), port).arg("ensure"));
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

    let (ok, _, stderr) = output(packset(home.path(), port).arg("stop"));
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = output(packset(home.path(), port).args(["status", "demo"]));
    assert!(!ok, "a stopped writer reported up: {stderr}");
    assert!(stderr.contains("packset ensure"), "{stderr}");
}

#[test]
fn a_taken_port_names_the_holder_and_the_next_command() {
    let home = tempfile::tempdir().unwrap();
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
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
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(&theirs)
        .env("PACKSET_AUTH", "off")
        .env("HOME", root.path())
        .env_remove("PACKSET_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_health(port);
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
    assert!(
        stderr.contains("packset stop && packset ensure"),
        "{stderr}"
    );
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
    assert!(
        stderr.contains("packset stop && packset ensure"),
        "{stderr}"
    );
    assert!(stderr.contains("PACKSET_HOME"), "{stderr}");
}

#[test]
fn a_missing_packsetd_names_the_packset_package() {
    let home = tempfile::tempdir().unwrap();
    let bin_dir = home.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let copied = bin_dir.join("packset");
    std::fs::copy(env!("CARGO_BIN_EXE_packset"), &copied).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&copied).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&copied, mode).unwrap();
    }
    let port = free_port();
    let (ok, _, stderr) = output(
        Command::new(&copied)
            .arg("ensure")
            .env("HOME", home.path())
            .env("PACKSET_HOME", home.path().join("pack"))
            .env("PACKSET_PORT", port.to_string())
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("PATH", "/usr/bin:/bin")
            .env_remove("PACKSET_URL")
            .env_remove("PACKSET_BIN"),
    );
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
    let port = free_port();
    let (ok, _, stderr) = output(packset(home.path(), port).arg("ensure"));
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
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = std::fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&path, mode).unwrap();
    }
    let out = Command::new(&path)
        .arg("ensure")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("cargo build --release -p packset"),
        "{stderr}"
    );
    assert!(!stderr.contains("packset-cli"), "{stderr}");
}
