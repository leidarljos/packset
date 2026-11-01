//! Read a pack home and print its records as JSON lines, for the interop check.
//!
//! `cargo run -p packset-daemon --example dump_store -- <home> [workspace]`

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let workspace = args.next();
    let store = packset_daemon::Store::open(std::path::Path::new(&root))?;
    let records = store.scan(workspace.as_deref())?;
    for record in records {
        println!("{}", serde_json::to_string(&record)?);
    }
    Ok(())
}
