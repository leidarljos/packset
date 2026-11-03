`cargo deny check` fails a `*` version on a registry crate.
It also fails a crate from a registry other than crates.io, and a git source other than milli, heed and lmdb-rs.
A path or git requirement in an unpublished crate is still allowed, which is the pinned milli dependency.
Duplicate versions stay a warning, because denying them fails on the older releases milli v0.41.3 keeps.
