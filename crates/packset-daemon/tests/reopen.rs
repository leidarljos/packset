//! A dropped writer keeps nothing: not the home's lock, not a descriptor,
//! not a thread.
//!
//! The descriptor and thread counts are the whole process's, and a fork
//! copies the whole process, so this is the only test in this file.

#![cfg(target_os = "linux")]

use packset_daemon::{Home, Service};

/// Five times glibc's `PTHREAD_KEYS_MAX` (1024). An open whose environment
/// outlived its drop kept that environment's thread key, and the open that
/// found no key left failed with EAGAIN.
const OPENS: usize = 5 * 1024;

fn entries(dir: &str) -> usize {
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

#[test]
fn a_dropped_writer_keeps_no_lock_no_descriptor_and_no_thread() {
    let parent = tempfile::tempdir().unwrap();

    // A child holds every descriptor its parent had until it execs. This
    // one never execs, so it holds them until it is killed.
    let forked = parent.path().join("forked");
    let service = Service::open(Home::new(&forked)).unwrap();
    // SAFETY: the child only waits for a signal, and `pause` and `_exit`
    // are async-signal-safe.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::pause();
            libc::_exit(0);
        }
    }
    assert!(child > 0, "fork: {}", std::io::Error::last_os_error());
    drop(service);
    let again = Service::open(Home::new(&forked));
    // SAFETY: `child` is this process's own child.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    drop(again.unwrap_or_else(|err| panic!("reopen beside a forked child: {err}")));

    let fds = entries("/proc/self/fd");
    let threads = entries("/proc/self/task");
    for n in 0..OPENS {
        let home = parent.path().join(n.to_string());
        let service = Service::open(Home::new(&home))
            .unwrap_or_else(|err| panic!("open {n} of {OPENS}: {err}"));
        drop(service);
        std::fs::remove_dir_all(&home).unwrap();
    }
    for n in 0..OPENS {
        let service = Service::open(Home::new(&forked))
            .unwrap_or_else(|err| panic!("reopen {n} of {OPENS}: {err}"));
        drop(service);
    }
    assert_eq!(
        entries("/proc/self/fd"),
        fds,
        "descriptors after {OPENS} opens"
    );
    assert_eq!(
        entries("/proc/self/task"),
        threads,
        "threads after {OPENS} opens"
    );
}
