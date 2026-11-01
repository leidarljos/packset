//! The writer keeps accepting after the process runs out of descriptors.
//!
//! This test lowers the open-file limit of its own process, so it is the
//! only test in this file.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use packset_daemon::wire::{self, Request, Response};

fn open_files() -> u64 {
    std::fs::read_dir("/proc/self/fd").map_or(0, |dir| dir.count() as u64)
}

fn ask(port: u16) -> std::io::Result<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

#[test]
fn accepting_resumes_once_descriptors_come_back() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        wire::serve(&listener, 1, 4, |_: &Request| Response::text(200, "up"));
    });
    assert!(ask(port).unwrap().starts_with("HTTP/1.1 200"));

    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource and a pointer to a live struct.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    let saved = limit;
    limit.rlim_cur = (open_files() + 16).min(limit.rlim_max);
    // SAFETY: as above; the soft limit stays under the hard one.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    // Files fill the process, one is let go, and a connection takes it: the
    // writer's accept for that connection finds no descriptor.
    let mut filler = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        filler.push(file);
    }
    filler.pop();
    let held = TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(held);
    drop(filler);
    // SAFETY: restoring the limits read above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &saved) }, 0);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if ask(port).is_ok_and(|out| out.starts_with("HTTP/1.1 200")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the writer did not answer after the descriptors came back"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
