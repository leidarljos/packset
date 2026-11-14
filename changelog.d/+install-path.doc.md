The checkout install is `cargo install --locked --path crates/packset-cli`.
`cargo install --locked packset` reaches crates.io and does not build this checkout. `cargo build --release -p packset` is that same package, and `bin/packset` runs the binary.
