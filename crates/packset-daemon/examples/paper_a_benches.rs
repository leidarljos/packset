//! Three named Paper A benches from in-repo fixtures.
//!
//! Writes go through `admit_seat_write`. A raw line is a refusal. The table
//! is a fixture measurement, not a published SOTA number.
//!
//! ```console
//! $ cargo run -p packset-daemon --example paper_a_benches
//! ```

use std::io::BufRead;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "data/paper_a_fixtures.jsonl".into()),
    );
    let file =
        std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut table = packset_core::PaperATable::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        table.add(&serde_json::from_str(&line)?);
    }
    print!("{}", table.table());
    Ok(())
}
