//! The first open after an upgrade. A pack written before `updated` is
//! stamped behind the listener: `packset ensure` answers within its budget
//! and a search answers while the pass runs. A disk that fills during the
//! pass leaves the writer up and the pack as it was, says how much room
//! the pass needs, and takes a write once there is room.

#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use heed::types::Bytes;
use heed::{Database, EnvOpenOptions};

/// `packset ensure`'s own startup budget, `STARTUP` in its main.
const ENSURE_BUDGET: Duration = Duration::from_secs(5);

/// The writer's map, packsetd's `MAP_SIZE`.
const MAP: usize = 1024 * 1024 * 1024;

/// The width of bge-small, the default encoder, so a record is the size a
/// seat's is. 50,000 of them are 394 MiB, which the writer at 947c498
/// stamped for 14.9 s before it bound, in a debug build on an 8-core Intel
/// Xeon, against a 5 s budget.
const DIMS: usize = 384;

/// `ts` as a link rewrite left it, and `valid_from` as the write left it.
const REWRITTEN: &str = "2026-10-18T19:31:19.444Z";
const WRITTEN: &str = "2026-10-16T10:07:21.644Z";

/// Free space left on the full disk: less than the 1 MiB one step of the
/// pass rewrites, and room for the writer's lock and token files.
const LEFT: u64 = 256 * 1024;

/// A seat's writer on its own port and home, stopped when the test ends.
struct Seat {
    port: u16,
    home: PathBuf,
    log: PathBuf,
}

impl Seat {
    fn new(home: PathBuf, log: PathBuf) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Self { port, home, log }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_packset"));
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.parent().unwrap())
            .env("PACKSET_HOME", &self.home)
            .env("PACKSET_PORT", self.port.to_string())
            .env("PACKSET_LOG", &self.log)
            .env("PACKSET_BIN", env!("CARGO_BIN_EXE_packsetd"));
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Wait for the writer to print a line holding `needle`.
    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let log = self.log();
            if let Some(line) = log.lines().find(|line| line.contains(needle)) {
                return line.to_string();
            }
            assert!(Instant::now() < deadline, "no {needle:?} in:\n{log}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let _ = self.run(&["stop"]);
    }
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Write `count` claims the way a writer from before `updated` left them,
/// each with a `dims`-wide vector, or none.
fn old_pack(home: &Path, count: usize, dims: usize) {
    let dir = home.join("memory.lmdb");
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: a fresh directory that no other process has open.
    let env = unsafe {
        EnvOpenOptions::new()
            .map_size(MAP)
            .max_dbs(1)
            .open(&dir)
            .unwrap()
    };
    let mut wtxn = env.write_txn().unwrap();
    let db: Database<Bytes, Bytes> = env.create_database(&mut wtxn, None).unwrap();
    let vector = if dims == 0 {
        "null".to_string()
    } else {
        let parts: Vec<String> = (0..dims)
            .map(|d| format!("{:.7}", (d as f64 * 0.618).fract() - 0.5))
            .collect();
        format!("[{}]", parts.join(","))
    };
    for i in 0..count {
        let id = format!("{i:032x}");
        let record = format!(
            r#"{{"schema":"inside.atom/v1","kind":"lesson","level":"explicit","text":"The parser number {i} settled the header question.","workspace":"w","origin":"user-declared","entities":["parser","n{i}"],"id":"{id}","ts":"{REWRITTEN}","valid_from":"{WRITTEN}","tombstone":false,"links":[],"due_at":"2026-12-01T00:00:00.000Z","review":{{"reps":0,"interval_s":86400,"ease":2.5,"stability":1.0,"difficulty":5.0,"last":"{WRITTEN}"}},"embedding":{vector}}}"#
        );
        db.put(&mut wtxn, format!("w\0{id}").as_bytes(), record.as_bytes())
            .unwrap();
    }
    wtxn.commit().unwrap();
}

#[test]
fn an_upgraded_pack_is_ready_within_the_ensure_budget_and_answers_while_it_is_stamped() {
    let dir = tempfile::tempdir().unwrap();
    let seat = Seat::new(dir.path().join("pack"), dir.path().join("packsetd.log"));
    old_pack(&seat.home, 50_000, DIMS);

    let started = Instant::now();
    let ensure = seat.run(&["ensure"]);
    let took = started.elapsed();
    assert!(ensure.status.success(), "{}", said(&ensure));
    assert!(
        said(&ensure).contains(&format!("PACKSET_URL=http://127.0.0.1:{}", seat.port)),
        "{}",
        said(&ensure)
    );
    assert!(took < ENSURE_BUDGET, "ensure took {took:?}");

    let search = seat.run(&["search", "--workspace", "w", "parser", "number", "4242"]);
    assert!(search.status.success(), "{}", said(&search));
    assert!(
        said(&search).contains("The parser number 4242 settled the header question."),
        "{}",
        said(&search)
    );
    assert!(
        !seat.log().contains("packsetd: stamped"),
        "the pass was over before the search answered:\n{}",
        seat.log()
    );

    seat.wait_for("packsetd: stamped 50000 records written before `updated`");
}

/// How the disk runs out of room during the pass.
enum Room {
    /// A small tmpfs, filled to [`LEFT`] free.
    Tmpfs,
    /// The writer's files capped at [`LEFT`] past the store, for a kernel
    /// that does not let this user mount a tmpfs.
    FileSize,
}

/// Set in the copy of this test that runs in its own mount namespace.
const IN_NAMESPACE: &str = "PACKSET_TEST_TMPFS";
const MOUNTED: &str = "tmpfs mounted for the full-disk test";

#[test]
fn a_full_disk_during_the_pass_keeps_the_writer_and_the_old_pack_and_takes_a_write_after() {
    if let Some(scratch) = std::env::var_os(IN_NAMESPACE) {
        let scratch = PathBuf::from(scratch);
        if let Err(err) = mount_tmpfs(&scratch.join("fs")) {
            eprintln!("no tmpfs here: {err}");
            return;
        }
        println!("{MOUNTED}");
        full_disk(&scratch, &Room::Tmpfs);
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    std::fs::create_dir(scratch.path().join("fs")).unwrap();
    let inside = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_full_disk_during_the_pass_keeps_the_writer_and_the_old_pack_and_takes_a_write_after",
            "--nocapture",
        ])
        .env(IN_NAMESPACE, scratch.path())
        .output();
    match inside {
        Ok(out) if String::from_utf8_lossy(&out.stdout).contains(MOUNTED) => {
            assert!(out.status.success(), "{}", said(&out));
        }
        _ => full_disk(scratch.path(), &Room::FileSize),
    }
}

/// 16 MiB, enough for the store and its files, so the filler is what fills it.
fn mount_tmpfs(at: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let target = std::ffi::CString::new(at.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated strings that outlive the call.
    let rc = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            target.as_ptr(),
            c"tmpfs".as_ptr(),
            0,
            c"size=16m".as_ptr().cast(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn full_disk(scratch: &Path, room: &Room) {
    let mut seat = Seat::new(scratch.join("fs/pack"), scratch.join("packsetd.log"));
    old_pack(&seat.home, 5_000, 0);
    let data = seat.home.join("memory.lmdb/data.mdb");
    let filler = scratch.join("fs/filler");
    let mut ensure = seat.command(&["ensure"]);
    match room {
        Room::Tmpfs => fill(&filler),
        Room::FileSize => {
            let cap = std::fs::metadata(&data).unwrap().len() + LEFT;
            cap_file_size(&mut ensure, cap);
        }
    }

    let ensure = ensure.output().unwrap();
    assert!(ensure.status.success(), "{}", said(&ensure));
    let line = seat.wait_for("records from before `updated` stay as they were");
    assert!(line.starts_with("packsetd: 5000 records"), "{line}");
    assert!(
        line.contains(&format!(
            "MiB free in {}",
            seat.home.join("memory.lmdb").display()
        )),
        "{line}"
    );
    assert!(line.contains("The next start tries again"), "{line}");

    let atoms = seat.run(&["atoms", "w"]);
    assert!(atoms.status.success(), "{}", said(&atoms));
    let stored = String::from_utf8_lossy(&atoms.stdout);
    assert_eq!(stored.lines().count(), 5_000);
    for atom in stored.lines() {
        assert!(!atom.contains("\"updated\""), "stamped: {atom}");
        assert!(atom.contains(&format!("\"ts\":\"{REWRITTEN}\"")), "{atom}");
    }
    let search = seat.run(&["search", "--workspace", "w", "parser", "number", "42"]);
    assert!(
        said(&search).contains("The parser number 42 settled the header question."),
        "{}",
        said(&search)
    );

    match room {
        Room::Tmpfs => std::fs::remove_file(&filler).unwrap(),
        Room::FileSize => uncap_file_size(seat.port),
    }
    let wrote = seat.run(&["remember", "--workspace", "w", "The disk has room again."]);
    assert!(wrote.status.success(), "{}", said(&wrote));

    let stop = seat.run(&["stop"]);
    assert!(stop.status.success(), "{}", said(&stop));
    seat.log = scratch.join("packsetd-again.log");
    let again = seat.run(&["ensure"]);
    assert!(again.status.success(), "{}", said(&again));
    seat.wait_for("packsetd: stamped 5000 records written before `updated`");
}

/// Fill the filesystem under `filler`, then free [`LEFT`] of it.
fn fill(filler: &Path) {
    use std::io::Write;
    let mut file = std::fs::File::create(filler).unwrap();
    let chunk = vec![0u8; usize::try_from(LEFT).unwrap()];
    let full = loop {
        if let Err(err) = file.write_all(&chunk) {
            break err;
        }
    };
    assert_eq!(full.raw_os_error(), Some(libc::ENOSPC), "{full}");
    let len = file.metadata().unwrap().len();
    file.set_len(len - LEFT).unwrap();
}

/// Start the writer with its files capped at `cap` bytes, and `SIGXFSZ`
/// ignored so a write past the cap fails rather than ending the process.
fn cap_file_size(cmd: &mut Command, cap: u64) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setrlimit and signal are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: cap,
                rlim_max: libc::RLIM_INFINITY,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            Ok(())
        });
    }
}

/// Lift the file-size cap from the writer on `port`.
fn uncap_file_size(port: u16) {
    let wanted = format!("--port\0{port}\0");
    let pid = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                cmdline.ends_with(wanted.as_bytes())
                    && String::from_utf8_lossy(&cmdline).contains("packsetd")
            })
        })
        .unwrap_or_else(|| panic!("no packsetd on {port}"));
    let limit = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: a limit this function owns, for a process of this user.
    let rc = unsafe { libc::prlimit(pid, libc::RLIMIT_FSIZE, &limit, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "prlimit: {}", std::io::Error::last_os_error());
}
