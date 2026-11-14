//! Who holds a loopback port, asked of the kernel rather than of `ss`.
//!
//! The lifecycle commands need two facts: whether the port is taken, and which
//! process took it. A connect answers the first. On Linux the second is a
//! socket inode in `/proc/net/tcp` and then a walk of `/proc/<pid>/fd` looking
//! for it, which is what `ss -ltnp` does and what `lsof` does, without either
//! being present. macOS has no `/proc`. There `lsof`, which the base system
//! ships, answers the pid, and `proc_name` answers the short name `comm`
//! would have given.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::time::Duration;

/// How long a probe waits before calling the port free.
const PROBE: Duration = Duration::from_millis(300);

/// `TCP_LISTEN`, the only state a bound server sits in.
#[cfg(target_os = "linux")]
const LISTEN: &str = "0A";

/// Whether anything accepts on `127.0.0.1:port`.
#[must_use]
pub fn listening(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE).is_ok()
}

/// The socket inodes listening on `port`, from one `/proc/net` table.
#[cfg(target_os = "linux")]
fn listening_inodes(table: &str, port: u16) -> Vec<u64> {
    let Ok(text) = fs::read_to_string(table) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        // sl, local_address, rem_address, st, ..., inode is the tenth column.
        let (Some(_), Some(local), Some(_), Some(state)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if state != LISTEN {
            continue;
        }
        let Some(hex_port) = local.rsplit(':').next() else {
            continue;
        };
        if u16::from_str_radix(hex_port, 16) != Ok(port) {
            continue;
        }
        // tx_queue rx_queue tr tm->when retrnsmt uid timeout inode
        if let Some(inode) = fields.nth(5).and_then(|raw| raw.parse::<u64>().ok()) {
            found.push(inode);
        }
    }
    found
}

/// Whether `pid` holds any of `inodes` as an open socket.
#[cfg(target_os = "linux")]
fn holds(pid: u32, inodes: &[u64]) -> bool {
    let dir = format!("/proc/{pid}/fd");
    let Ok(entries) = fs::read_dir(&dir) else {
        // Another user's process, or one that exited while we looked.
        return false;
    };
    for entry in entries.flatten() {
        let Ok(meta) = fs::metadata(entry.path()) else {
            continue;
        };
        // A socket fd stats as the socket, so the inode matches without having
        // to parse the `socket:[N]` link text.
        if meta.file_type().is_socket() && inodes.contains(&meta.ino()) {
            return true;
        }
    }
    false
}

/// The process listening on `port`, when it is one this user can see.
///
/// `None` covers three cases a caller treats alike: nothing is listening, the
/// holder belongs to another user, or it exited mid-walk.
#[cfg(target_os = "linux")]
#[must_use]
pub fn pid_on_port(port: u16) -> Option<u32> {
    let mut inodes = listening_inodes("/proc/net/tcp", port);
    inodes.extend(listening_inodes("/proc/net/tcp6", port));
    if inodes.is_empty() {
        return None;
    }
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|raw| raw.parse::<u32>().ok()) else {
            continue;
        };
        if holds(pid, &inodes) {
            return Some(pid);
        }
    }
    None
}

/// The process listening on `port`, as `lsof` reports it.
///
/// `None` covers the cases it does on Linux, and a missing `lsof`. macOS
/// keeps it in `/usr/sbin`, which the login and launchd `PATH` both carry.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn pid_on_port(port: u16) -> Option<u32> {
    use std::process::{Command, Stdio};
    // -t prints bare pids, -n and -P skip the host and service lookups, and
    // -sTCP:LISTEN keeps the bound socket and not a client of it.
    let out = Command::new("lsof")
        .args(["-t", "-n", "-P", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.trim().parse().ok())
}

/// The short name of `pid`, from the kernel.
///
/// Linux reads `comm`. macOS asks `proc_name`, which is the same field.
/// `None` when the process is gone, or on a host that has neither.
#[must_use]
pub fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        let name = text.trim();
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    }
    #[cfg(target_os = "macos")]
    {
        // `p_comm` is `MAXCOMLEN` (16) bytes. A wider buffer still comes
        // back that short, and the extra room keeps a longer `pbi_name`.
        let mut buf = [0u8; 32];
        // SAFETY: `buf` is writable for the length passed. `proc_name`
        // writes a NUL-terminated name, or nothing when `pid` is gone.
        let n = unsafe {
            libc::proc_name(pid.cast_signed(), buf.as_mut_ptr().cast(), buf.len() as u32)
        };
        if n <= 0 {
            return None;
        }
        let n = (n as usize).min(buf.len());
        let end = buf.iter().take(n).position(|&b| b == 0).unwrap_or(n);
        let name = std::str::from_utf8(&buf[..end]).ok()?.trim();
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Ask `pid` to stop.
///
/// # Errors
///
/// The kernel's, when the process is gone or belongs to another user.
pub fn terminate(pid: u32) -> io::Result<()> {
    // SAFETY: kill with a valid signal number; the pid was just read back as
    // the port's holder, and a stale one fails with ESRCH rather than
    // reaching anything.
    let rc = unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether the path names a file this user can execute.
#[must_use]
pub fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    mod support {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_support/harness.rs"
        ));
    }

    #[test]
    fn a_port_nobody_bound_is_not_listening() {
        // Below the ephemeral range, and not issued again, so another test's
        // bind(0) cannot land on it once this probe has closed the socket.
        let port = support::free_port();
        assert!(!listening(port));
    }

    #[test]
    fn the_holder_of_a_port_is_this_process() {
        let socket = support::bind();
        let port = socket.local_addr().unwrap().port();
        assert!(listening(port));
        let pid = pid_on_port(port);
        // Linux reads the socket inode out of `/proc`. macOS asks `lsof`
        // and `proc_name`. Both name this process. A host with neither
        // leaves the holder unnamed, and the caller says another process
        // has the port.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert_eq!(pid, Some(std::process::id()));
            let name = process_name(std::process::id()).expect("this process has no name");
            assert!(!name.is_empty(), "{name}");
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            if let Some(found) = pid {
                assert_eq!(found, std::process::id());
            }
            assert!(process_name(std::process::id()).is_none());
        }
    }
}
