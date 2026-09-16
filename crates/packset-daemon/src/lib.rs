//! The loopback pack writer.
//!
//! One process owns `memory.lmdb` and every client speaks HTTP to it, so an
//! isolated harness home does not get a private store. Cards stay files
//! because a person edits them; atoms are a database because a program does.

pub mod cards;
pub mod context;
pub mod embed;
pub mod glob;
pub mod home;
pub mod http;
pub mod milli;
pub mod proposals;
pub mod service;
pub mod store;
pub mod workspace;

pub use home::Home;
pub use service::Service;
pub use store::Store;

/// The `packsetd` binary. Also the `packset` crate's packsetd bin.
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
    http::serve(service, panel, &host, port)
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
             --decay <name>      host decay voter, or PACKSET_DECAY",
        http::LOOPBACK,
        http::DEFAULT_PORT
    )
}
