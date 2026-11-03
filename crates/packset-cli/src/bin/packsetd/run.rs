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
            // `embed::enabled` reads this argument. The arm is what keeps
            // the writer from refusing the flag as unknown.
            "--no-embed" => {}
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
    if owned {
        watch_parent(Arc::clone(&service), parent_signal);
    }
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
             --no-embed          leave the encoder unstarted; INSIDE_EMBED=off\n\
         \n\
         Every request but GET /health carries the token in <home>/token as\n\
         Authorization: Bearer TOKEN. PACKSET_AUTH=off drops the check.",
        http::LOOPBACK,
        http::DEFAULT_PORT
    )
}

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
        assert!(usage.contains("INSIDE_EMBED=off"), "{usage}");
        assert!(
            usage.contains("0 prints and writes the assigned port"),
            "{usage}"
        );
    }
}
