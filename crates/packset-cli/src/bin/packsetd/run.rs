//! The writer's command line: flags, the panel, then the listener.

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
        .or_else(|_| std::env::var("GROK_MEM_PORT"))
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(http::DEFAULT_PORT);
    let mut root: Option<std::path::PathBuf> = None;
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
            "--home" => root = Some(next("--home")?.into()),
            "--fuse" => fuse = Some(next("--fuse")?),
            "--diversify" => diversify = Some(next("--diversify")?),
            "--decay" => decay = Some(next("--decay")?),
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

    let service = Arc::new(Service::open(Home::new(&root))?);
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
             --port <n>          default {}, or PACKSET_PORT\n\
             --home <dir>        the pack home, or PACKSET_HOME\n\
             --fuse <name>       host fuse voter, or PACKSET_FUSE\n\
             --diversify <name>  host diversify voter, or PACKSET_DIVERSIFY\n\
             --decay <name>      host decay voter, or PACKSET_DECAY\n\
         \n\
         Every request but GET /health carries the token in <home>/token as\n\
         Authorization: Bearer TOKEN. PACKSET_AUTH=off drops the check.",
        http::LOOPBACK,
        http::DEFAULT_PORT
    )
}
