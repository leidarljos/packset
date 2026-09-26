root := justfile_directory()

# Everything but the search projection, which needs its own toolchain.
check:
    cargo fmt --all --check
    cargo clippy --locked --workspace --exclude packset-milli --exclude packset-embed --all-targets -- -D warnings
    cargo test --locked --workspace --exclude packset-milli --exclude packset-embed --no-fail-fast

# What a pack costs as it grows, and the shape of the graph inside it.
bench sizes="":
    cargo run --release -p packset-daemon --example bench -- {{sizes}}

milli:
    #!/usr/bin/env bash
    set -euo pipefail
    # The builder is the host where the model may live; it says so with
    # PACKSET_BUILDER=1 in its environment rather than by name.
    if [ "${PACKSET_BUILDER:-}" != 1 ]; then
        echo "just milli: build on the builder (PACKSET_BUILDER=1), not here" >&2
        exit 1
    fi
    cargo build -p packset-milli --release

# The dense projection. Same rule as milli: it carries a native runtime and a
# model download, so it is built where the model is allowed to live.
embed:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${PACKSET_BUILDER:-}" != 1 ]; then
        echo "just embed: build on the builder (PACKSET_BUILDER=1), not here" >&2
        exit 1
    fi
    cargo build -p packset-embed --release

ensure:
    bin/packset ensure

# MemoryAgentBench write protocol: Remember / Prefer / Accept, four competencies.
mab dir="data/mab":
    cargo run --release -p packset-daemon --example memoryagentbench -- {{dir}}
