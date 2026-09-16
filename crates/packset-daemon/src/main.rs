//! `packsetd`: the loopback pack writer.

fn main() -> anyhow::Result<()> {
    packset_daemon::run()
}
