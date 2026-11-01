//! Write JSON-line records into a pack home, for the interop check.
//!
//! `cargo run -p packset-daemon --example write_store -- <home> < records.jsonl`

use std::io::Read;

fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let store = packset_daemon::Store::open(std::path::Path::new(&root))?;
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)?;
        let record = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("each line must be an object"))?;
        store.upsert(record)?;
    }
    Ok(())
}
