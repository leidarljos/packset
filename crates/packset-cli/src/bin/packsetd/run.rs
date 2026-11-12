//! The writer's command line: flags, the panel, then the listener.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{http, Home, Service};

/// The `packsetd` binary, whichever package builds it.
///
/// # Errors
///
/// Fails when flags are unknown, the store cannot open, or the listener
/// cannot bind.
pub fn run() -> anyhow::Result<()> {
    use std::sync::Arc;

    let mut host = http::LOOPBACK.to_string();
    let mut port = std::env::var("PACKSET_PORT")
        .or_else(|_| std::env::var("LJOS_MEM_PORT"))
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(http::DEFAULT_PORT);
    let mut root: Option<std::path::PathBuf> = None;
    let mut fuse = None;
    let mut diversify = None;
    let mut decay = None;
    let mut owned = false;
    let mut supervise = false;
    let mut cache_mb: Option<usize> = None;
    let mut embed_idle_min: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = |flag: &str| -> anyhow::Result<String> {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--host" => host = next("--host")?,
            "--port" => port = next("--port")?.parse()?,
            "--home" => root = Some(next("--home")?.into()),
            "--fuse" => fuse = Some(next("--fuse")?),
            "--diversify" => diversify = Some(next("--diversify")?),
            "--decay" => decay = Some(next("--decay")?),
            "--owned" => owned = true,
            "--supervise" => supervise = true,
            // `embed::enabled` reads this argument. The arm is what keeps
            // the writer from refusing the flag as unknown.
            "--no-embed" => {}
            "--cache-mb" => cache_mb = Some(next("--cache-mb")?.parse()?),
            "--embed-idle-min" => embed_idle_min = Some(next("--embed-idle-min")?.parse()?),
            "-h" | "--help" => {
                println!("{}", packsetd_usage());
                return Ok(());
            }
            "-V" | "--version" => {
                println!(
                    "packsetd {} ({})",
                    env!("CARGO_PKG_VERSION"),
                    env!("PACKSET_COMMIT")
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{}", packsetd_usage()),
        }
    }

    // The parent stays small and starts the writer again when the child is
    // killed. It does not open the store: that is what the child is for.
    if supervise {
        return respawn();
    }
    bound_allocator();

    // Arm the parent-death signal before any thread exists, so workers
    // inherit the blocked mask and the default action cannot fire first.
    // The pipe covers macOS, where there is no such signal, and Linux if
    // the signal is lost. The signal covers a parent that died while some
    // other process still holds the write end.
    #[cfg(target_os = "linux")]
    let parent_signal = owned && arm_parent_death_signal();
    #[cfg(not(target_os = "linux"))]
    let parent_signal = false;

    // Resolved after the flags, so `--home` skips the default and its
    // notice about an old home this writer will not open.
    let root = root
        .or_else(packset_core::home::named_home)
        .unwrap_or_else(Home::default_root);
    let fuse = fuse.or_else(|| std::env::var("PACKSET_FUSE").ok());
    let diversify = diversify.or_else(|| std::env::var("PACKSET_DIVERSIFY").ok());
    let decay = decay.or_else(|| std::env::var("PACKSET_DECAY").ok());
    let panel = packset_core::Panel::from_env_vars(
        fuse.as_deref(),
        diversify.as_deref(),
        decay.as_deref(),
    )?;
    eprintln!(
        "packsetd: panel {} / {} / {}",
        panel.fuse.as_str(),
        panel.diversify.as_str(),
        panel.decay.as_str()
    );
    if !crate::embed::enabled() {
        eprintln!("packsetd: encoder off; search is lexical");
    }

    let service = Arc::new(Service::open(Home::new(&root))?);
    if let Some(mebibytes) = cache_mb {
        service
            .store()
            .set_cache_limit_bytes(mebibytes.saturating_mul(1024 * 1024));
    }
    if let Some(minutes) = embed_idle_min {
        crate::embed::set_idle_minutes(minutes);
    }
    if owned {
        watch_parent(Arc::clone(&service), parent_signal);
    }
    // An upgraded pack is stamped behind the listener, so `packset ensure`
    // does not wait on it and its failure does not stop the writer.
    let stamping = Arc::clone(&service);
    std::thread::spawn(move || stamping.store().restore_created());
    // Every user on the host reaches loopback; the token is what keeps the
    // pack its owner's. `PACKSET_AUTH=off` is for a client that cannot send it.
    let token = if std::env::var("PACKSET_AUTH").is_ok_and(|v| v.trim() == "off") {
        eprintln!("packsetd: PACKSET_AUTH=off; every local user can read and write this pack");
        None
    } else {
        Some(crate::auth::ensure_token(&root)?)
    };
    http::serve(service, panel, &host, port, token)
}

fn packsetd_usage() -> String {
    format!(
        "packsetd: the loopback pack writer\n\
         \n\
             -V, --version       the build this is\n\
             --host <addr>       {} only, which is the contract\n\
             --port <n>          default {}, or PACKSET_PORT; 0 prints and writes the assigned port\n\
             --home <dir>        the pack home, or PACKSET_HOME\n\
             --fuse <name>       host fuse voter, or PACKSET_FUSE\n\
             --diversify <name>  host diversify voter, or PACKSET_DIVERSIFY\n\
             --decay <name>      host decay voter, or PACKSET_DECAY\n\
             --owned             exit when stdin closes; Linux also signals this process when its parent dies\n\
             --supervise         restart this process when it dies; a stop signal ends the loop\n\
             --no-embed          leave the encoder unstarted; INSIDE_EMBED=off\n\
             --cache-mb <n>      cap on cached atoms, indexes and snapshots, in MiB; default 128, or PACKSET_CACHE_MB; 0 is no cap\n\
             --embed-idle-min <n> stop the encoder after n minutes without a call; default 5, or PACKSET_EMBED_IDLE_MIN; 0 leaves it up\n\
         \n\
         Every request but GET /health carries the token in <home>/token as\n\
         Authorization: Bearer TOKEN. PACKSET_AUTH=off drops the check.",
        http::LOOPBACK,
        http::DEFAULT_PORT
    )
}

/// How a child ended, for [`respawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildStop {
    /// Exit 0: help, version, or a clean stop.
    Clean,
    /// SIGINT or SIGTERM: someone asked it to stop. `packset stop` sends
    /// SIGTERM to the process on the port.
    Stopped,
    /// A crash, or SIGKILL. The OOM killer sends SIGKILL.
    Died,
}

/// Whether [`respawn`] should start the child again.
fn restart_after(stop: ChildStop) -> bool {
    matches!(stop, ChildStop::Died)
}

/// Classify a child status. A signalled stop is not a crash.
fn classify_status(status: std::process::ExitStatus) -> ChildStop {
    if status.success() {
        return ChildStop::Clean;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            Some(2 | 15) => return ChildStop::Stopped,
            Some(_) => return ChildStop::Died,
            None => {}
        }
    }
    ChildStop::Died
}

/// Run the child until it stops cleanly or is asked to stop. A death starts
/// another child after `wait`. The pause starts at 200 ms and doubles, up to
/// 5 s, and a child that ran for 30 s starts the pause over.
fn supervise_loop(
    mut run_child: impl FnMut() -> ChildStop,
    mut wait: impl FnMut(std::time::Duration),
) {
    let mut pause = std::time::Duration::from_millis(200);
    loop {
        let started = std::time::Instant::now();
        let stop = run_child();
        if !restart_after(stop) {
            return;
        }
        if started.elapsed() >= std::time::Duration::from_secs(30) {
            pause = std::time::Duration::from_millis(200);
        }
        wait(pause);
        pause = (pause * 2).min(std::time::Duration::from_secs(5));
    }
}

/// Start this binary again, without `--supervise`, until it is stopped.
fn respawn() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let child_args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--supervise")
        .collect();
    eprintln!("packsetd: supervising; the writer restarts if it is killed");
    supervise_loop(
        || {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(&child_args);
            match cmd.status() {
                Ok(status) => {
                    let stop = classify_status(status);
                    if restart_after(stop) {
                        eprintln!("packsetd: exited {status}");
                    }
                    stop
                }
                Err(err) => {
                    eprintln!("packsetd: could not start: {err}");
                    ChildStop::Died
                }
            }
        },
        |pause| {
            eprintln!("packsetd: starting again in {} ms", pause.as_millis());
            std::thread::sleep(pause);
        },
    );
    Ok(())
}

/// Cap glibc's arenas and hand large freed buffers back. Each worker would
/// otherwise keep its own 64 MiB arena after a listing.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn bound_allocator() {
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 128 * 1024);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn bound_allocator() {}

/// Read stdin until it closes, then flush and exit.
///
/// `signal_armed` is the Linux parent-death signal. The pipe is what macOS
/// has, and what Linux uses as well: a parent killed by any signal has its
/// descriptors closed by the kernel, and this read returns.
fn watch_parent(service: Arc<Service>, signal_armed: bool) {
    let once = Arc::new(AtomicBool::new(false));
    let for_pipe = Arc::clone(&service);
    let once_pipe = Arc::clone(&once);
    if let Err(err) = std::thread::Builder::new()
        .name("packsetd-parent".to_string())
        .spawn(move || {
            read_parent_pipe();
            leave(for_pipe, &once_pipe);
        })
    {
        eprintln!("packsetd: cannot watch the parent pipe: {err}");
    }
    watch_parent_signal(service, once, signal_armed);
}

/// The parent writes nothing. One byte is enough to observe the close; a
/// byte that did arrive is discarded so the next read can see EOF.
fn read_parent_pipe() {
    let mut input = std::io::stdin().lock();
    let mut buf = [0u8; 1];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("packsetd: parent pipe: {err}");
                return;
            }
        }
    }
}

/// Flush the store and end the process. Two watchers can arrive together;
/// one flushes, the other waits here until that exit takes it.
fn leave(service: Arc<Service>, once: &AtomicBool) -> ! {
    if once
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        loop {
            std::thread::park();
        }
    }
    if let Err(err) = service.store().sync() {
        eprintln!("packsetd: flush: {err}");
    }
    service.flush_projection();
    eprintln!("packsetd: the parent exited; flushed and exiting");
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
fn watch_parent_signal(service: Arc<Service>, once: Arc<AtomicBool>, signal_armed: bool) {
    if !signal_armed {
        return;
    }
    if std::thread::Builder::new()
        .name("packsetd-parent-signal".to_string())
        .spawn(move || {
            wait_for_parent_signal();
            leave(service, &once);
        })
        .is_err()
    {
        // SIGTERM is blocked and nobody is waiting for it. Unblock so a
        // later signal still ends the process; the pipe remains the flush.
        unblock_sigterm();
    }
}

#[cfg(not(target_os = "linux"))]
fn watch_parent_signal(service: Arc<Service>, once: Arc<AtomicBool>, signal_armed: bool) {
    let _ = (service, once, signal_armed);
}

/// Block `SIGTERM` and ask to be sent it when the parent dies.
///
/// The signal is blocked so its default action does not skip the flush.
/// A parent that dies between the two `getppid` calls is raised into the
/// same blocked queue, and the waiter takes it.
///
/// # Returns
///
/// Whether `SIGTERM` is now blocked. The caller starts the waiter, and
/// unblocks if that thread does not start.
#[cfg(target_os = "linux")]
fn arm_parent_death_signal() -> bool {
    if !block_sigterm() {
        return false;
    }
    // SAFETY: PR_SET_PDEATHSIG takes the signal number in the second
    // argument and ignores the rest. The mask above keeps the signal
    // pending until `sigwait`. `getppid` and `raise` are the race check
    // from prctl(2): the parent can die after we read its id and before
    // the kernel records the signal.
    unsafe {
        let parent = libc::getppid();
        let armed = libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0,
            0,
            0,
        );
        if armed == -1 {
            eprintln!(
                "packsetd: parent-death signal was not armed: {}",
                std::io::Error::last_os_error()
            );
        } else if libc::getppid() != parent {
            libc::raise(libc::SIGTERM);
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn block_sigterm() -> bool {
    // SAFETY: the set is initialised by `sigemptyset` before it is read.
    // Blocking SIGTERM in this thread is inherited by every thread spawned
    // afterwards, which is all of them: this runs before the pool.
    unsafe {
        let mut set = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 || libc::sigaddset(&mut set, libc::SIGTERM) != 0 {
            eprintln!(
                "packsetd: could not block SIGTERM: {}",
                std::io::Error::last_os_error()
            );
            return false;
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            eprintln!(
                "packsetd: could not block SIGTERM: {}",
                std::io::Error::last_os_error()
            );
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn unblock_sigterm() {
    // SAFETY: same set as `block_sigterm`, used only to undo it.
    unsafe {
        let mut set = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 || libc::sigaddset(&mut set, libc::SIGTERM) != 0 {
            return;
        }
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Sit in `sigwait` until the parent-death signal arrives.
#[cfg(target_os = "linux")]
fn wait_for_parent_signal() {
    // SAFETY: `sigwait` requires the signal to be blocked in this thread,
    // which `arm_parent_death_signal` did before this thread was spawned.
    unsafe {
        let mut set = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 || libc::sigaddset(&mut set, libc::SIGTERM) != 0 {
            return;
        }
        loop {
            let mut sig = 0;
            if libc::sigwait(&set, &mut sig) == 0 && sig == libc::SIGTERM {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_names_the_encoder_switch_and_port_zero() {
        let usage = packsetd_usage();
        assert!(usage.contains("--no-embed"), "{usage}");
        assert!(usage.contains("--cache-mb"), "{usage}");
        assert!(usage.contains("--embed-idle-min"), "{usage}");
        assert!(usage.contains("INSIDE_EMBED=off"), "{usage}");
        assert!(
            usage.contains("0 prints and writes the assigned port"),
            "{usage}"
        );
        assert!(usage.contains("--supervise"), "{usage}");
    }

    #[test]
    fn a_crash_is_started_again_and_a_stop_is_not() {
        assert!(restart_after(ChildStop::Died));
        assert!(!restart_after(ChildStop::Stopped));
        assert!(!restart_after(ChildStop::Clean));
        let mut starts = 0;
        let mut waits = Vec::new();
        supervise_loop(
            || {
                starts += 1;
                if starts < 3 {
                    ChildStop::Died
                } else {
                    ChildStop::Clean
                }
            },
            |pause| waits.push(pause),
        );
        assert_eq!(starts, 3);
        assert_eq!(
            waits,
            vec![
                std::time::Duration::from_millis(200),
                std::time::Duration::from_millis(400),
            ]
        );
        let mut starts = 0;
        supervise_loop(
            || {
                starts += 1;
                ChildStop::Stopped
            },
            |_| panic!("a stop signal must not wait to restart"),
        );
        assert_eq!(starts, 1);
    }
}
