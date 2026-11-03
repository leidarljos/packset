CI runs `cargo deny check` in full, for advisories, bans, licenses and sources, on every push and pull request and each Monday on main and develop.
`just check` runs clippy on each writer crate alone, as CI does.
