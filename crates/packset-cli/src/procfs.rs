//! Who holds a loopback port, asked of the kernel rather than of `ss`.
//!
//! The lifecycle commands need two facts: whether the port is taken, and which
//! process took it. A connect answers the first. The second is a socket inode
//! in `/proc/net/tcp` and then a walk of `/proc/<pid>/fd` looking for it, which
//! is what `ss -ltnp` does and what `lsof` does, without either being present.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::time::Duration;

/// How long a probe waits before calling the port free.
const PROBE: Duration = Duration::from_millis(300);

/// `TCP_LISTEN`, the only state a bound server sits in.
const LISTEN: &str = "0A";

/// Whether anything accepts on `127.0.0.1:port`.
#[must_use]
pub fn listening(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE).is_ok()
}

/// The socket inodes listening on `port`, from one `/proc/net` table.
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

/// Ask `pid` to stop.
///
/// # Errors
///
/// The kernel's, when the process is gone or belongs to another user.
pub fn terminate(pid: u32) -> io::Result<()> {
    // SAFETY: kill with a valid signal number; the pid is read back from
    // /proc, and a stale one fails with ESRCH rather than reaching anything.
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
    use std::net::TcpListener;

    #[test]
    fn a_port_nobody_bound_is_not_listening() {
        // Bind to learn a free port, then drop it: the number is now free and
        // the kernel has not handed it out again.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!listening(port));
    }

    #[test]
    fn the_holder_of_a_port_is_this_process() {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert!(listening(port));
        assert_eq!(pid_on_port(port), Some(std::process::id()));
    }
}
