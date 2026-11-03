The bench example builds on its own again, with `cargo run -p packset-daemon --example bench`.
CI now runs clippy on each writer crate alone as well as on the workspace, so a target that compiles only on another crate's features fails there.
