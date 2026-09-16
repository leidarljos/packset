//! `packsetd` shipped from the `packset` crate so `cargo install packset`
//! installs the writer.

fn main() -> anyhow::Result<()> {
    packset_daemon::run()
}
