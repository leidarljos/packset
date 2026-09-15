//! `packsetd`: the loopback pack writer.
//!
//! One process owns the store and every client speaks HTTP to it, so an
//! isolated harness home does not get a private store. Cards stay files
//! because a person edits them; atoms are a database because a program does.

use std::sync::Arc;

use packset_daemon::{http, Home, Service};

fn main() -> anyhow::Result<()> {
    let mut host = http::LOOPBACK.to_string();
    let mut port = std::env::var("PACKSET_PORT")
        .or_else(|_| std::env::var("GROK_MEM_PORT"))
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(http::DEFAULT_PORT);
    // `PACKSET_HOME` is the name the rest of the pack's variables share;
    // the older names still answer.
    let mut root = std::env::var_os("PACKSET_HOME")
        .or_else(|| std::env::var_os("GROKINSIDE_HOME"))
        .or_else(|| std::env::var_os("GROK_INSIDE_MEMORY_HOME"))
        .map_or_else(Home::default_root, Into::into);
    let mut fuse = None;
    let mut diversify = None;
    let mut decay = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = |flag: &str| -> anyhow::Result<String> {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--host" => host = next("--host")?,
            "--port" => port = next("--port")?.parse()?,
            "--home" => root = next("--home")?.into(),
            "--fuse" => fuse = Some(next("--fuse")?),
            "--diversify" => diversify = Some(next("--diversify")?),
            "--decay" => decay = Some(next("--decay")?),
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(());
            }
            // Answered before the store is opened, so anything checking for
            // drift can ask a build that cannot take the lock.
            "-V" | "--version" => {
                println!(
                    "packsetd {} ({})",
                    env!("CARGO_PKG_VERSION"),
                    env!("PACKSET_COMMIT")
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{}", usage()),
        }
    }

    // The panel is named at the host, never by a client, and an unknown name
    // fails closed here rather than silently falling back mid-search. A flag
    // wins over the variable of the same name; nothing named is the default.
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

    let service = Arc::new(Service::open(Home::new(root))?);
    // Do not pre-spawn encoder children. Each one is a model in RAM. The
    // first search starts one query encoder; a document encoder starts
    // only if a dense ballot needs it.
    http::serve(service, panel, &host, port)
}

fn usage() -> String {
    format!(
        "packsetd: the loopback pack writer\n\
         \n\
             -V, --version       the build this is\n\
             --host <addr>       {} only, which is the contract\n\
             --port <n>          default {}, or PACKSET_PORT\n\
             --home <dir>        the pack home, or PACKSET_HOME\n\
             --fuse <name>       host fuse voter, or PACKSET_FUSE\n\
             --diversify <name>  host diversify voter, or PACKSET_DIVERSIFY\n\
             --decay <name>      host decay voter, or PACKSET_DECAY",
        http::LOOPBACK,
        http::DEFAULT_PORT
    )
}
