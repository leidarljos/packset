// Ports and executables for tests that would otherwise share a process-wide
// resource.
//
// A port taken from `bind(127.0.0.1:0)` and then dropped is free for the
// next caller, including another test in this process. Two tests then
// speak to one writer. Ports here sit below the kernel's ephemeral range,
// so that `bind(0)` cannot be handed one of them, and a lock file keeps
// the counter from issuing the same port twice.
//
// `ETXTBSY` is a fork that inherited the write end of a script this test
// is about to exec. The bytes are written aside, synced and closed, then
// renamed into place. A spawn that still races that window is retried.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::Duration;

const PORT_FLOOR: u16 = 10_000;

/// Bind one reserved loopback port and keep the socket.
pub fn bind() -> TcpListener {
    let mut held = lock();
    let (lo, hi) = port_bounds();
    let mut last = read_last(&mut held.file);
    let span = u32::from(hi - lo) + 1;
    for _ in 0..span {
        last = if last < lo || last >= hi { lo } else { last + 1 };
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, last)) {
            write_last(&mut held.file, last);
            return listener;
        }
    }
    panic!("no free loopback port in {lo}-{hi}");
}

/// One reserved port, with the socket already closed.
///
/// The number is not issued again. [`with_port`] is the one to use when a
/// child has to bind that port: it holds the lock across the hand-off.
pub fn free_port() -> u16 {
    let listener = bind();
    listener
        .local_addr()
        .map(|addr| addr.port())
        .expect("a bound socket has a port")
}

/// A port reserved for `body`.
///
/// The socket stays open until `body` starts, and the port lock is held
/// until `body` returns. `body` is what binds the port for real, or what
/// gives up without binding. It must not call [`bind`], [`free_port`], or
/// [`with_port`]: the lock is not re-entrant.
pub fn with_port<T>(body: impl FnOnce(u16) -> T) -> T {
    let listener = bind();
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .expect("a bound socket has a port");
    let _held = lock();
    drop(listener);
    body(port)
}

/// Write `bytes` to `path` and close every descriptor before returning.
///
/// The write goes to a temporary file in the same directory. That file is
/// synced, closed, marked executable, and renamed onto `path`, so the path
/// a later exec uses was not open for write in this process.
pub fn write_executable(path: &Path, bytes: impl AsRef<[u8]>) {
    let tmp = temp_beside(path);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .unwrap_or_else(|err| panic!("create {}: {err}", tmp.display()));
        file.write_all(bytes.as_ref())
            .unwrap_or_else(|err| panic!("write {}: {err}", tmp.display()));
        file.sync_all()
            .unwrap_or_else(|err| panic!("sync {}: {err}", tmp.display()));
        set_executable(&file);
    }
    fs::rename(&tmp, path).unwrap_or_else(|err| {
        let _ = fs::remove_file(&tmp);
        panic!("rename {} to {}: {err}", tmp.display(), path.display())
    });
}

/// Copy `from` onto `to` the way [`write_executable`] writes bytes.
pub fn copy_executable(from: &Path, to: &Path) {
    let bytes = fs::read(from).unwrap_or_else(|err| panic!("read {}: {err}", from.display()));
    write_executable(to, bytes);
}

/// [`Command::spawn`], retried while the kernel reports the text file busy.
pub fn spawn(cmd: &mut Command) -> io::Result<Child> {
    retry(|| cmd.spawn())
}

/// [`Command::output`], retried while the kernel reports the text file busy.
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    retry(|| cmd.output())
}

fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut pause = Duration::from_millis(1);
    let mut last = None;
    for _ in 0..30 {
        match op() {
            Err(err) if text_busy(&err) => {
                last = Some(err);
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(25));
            }
            other => return other,
        }
    }
    Err(last.expect("the busy retry ran"))
}

fn text_busy(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::ExecutableFileBusy || err.raw_os_error() == Some(26)
}

fn set_executable(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = file
            .metadata()
            .expect("metadata of the executable being written")
            .permissions();
        mode.set_mode(0o755);
        file.set_permissions(mode)
            .expect("mark the executable runnable");
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bin".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = format!(".{name}.{}.{nanos}.tmp", std::process::id());
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

/// Below the kernel's ephemeral range, so `bind(0)` is not assigned one of
/// these ports. Linux publishes the range; other hosts start it at 49152.
fn port_bounds() -> (u16, u16) {
    #[cfg(target_os = "linux")]
    if let Some((lo, hi)) = linux_ephemeral() {
        if lo > PORT_FLOOR.saturating_add(1) {
            return (PORT_FLOOR, lo - 1);
        }
        if hi < 65_000 {
            return (hi + 1, 65_000);
        }
    }
    (PORT_FLOOR, 32_767)
}

#[cfg(target_os = "linux")]
fn linux_ephemeral() -> Option<(u16, u16)> {
    let text = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
    let mut parts = text.split_whitespace();
    let lo = parts.next()?.parse().ok()?;
    let hi = parts.next()?.parse().ok()?;
    (lo < hi).then_some((lo, hi))
}

fn lock_path() -> PathBuf {
    std::env::temp_dir().join("packset-test-ports")
}

fn open_lock() -> File {
    let path = lock_path();
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap_or_else(|err| panic!("open {}: {err}", path.display()))
}

fn read_last(file: &mut File) -> u16 {
    file.seek(SeekFrom::Start(0))
        .expect("seek the port counter");
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .expect("read the port counter");
    buf.trim().parse().unwrap_or(0)
}

fn write_last(file: &mut File, port: u16) {
    file.seek(SeekFrom::Start(0))
        .expect("seek the port counter");
    file.set_len(0).expect("truncate the port counter");
    writeln!(file, "{port}").expect("write the port counter");
    file.flush().expect("flush the port counter");
}

struct LockGuard {
    file: File,
}

fn lock() -> LockGuard {
    let file = open_lock();
    lock_exclusive(&file);
    LockGuard { file }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

fn lock_exclusive(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        loop {
            // SAFETY: `fd` belongs to `file`. `flock` waits until this
            // process holds the lock, or fails and sets errno.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if rc == 0 {
                return;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                panic!("lock {}: {err}", lock_path().display());
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

fn unlock(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `fd` belongs to `file`, which this call locked.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        if rc != 0 {
            panic!("unlock {}: {}", lock_path().display(), io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}
