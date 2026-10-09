# Seat pack

The seat pack (`USER.md`, `MEMORY.md`, atoms) is not this tree.
Do not embed git-tracked files into memory.

## Commands

- `just check` — fmt, clippy, and the workspace tests
- `just bench` — what a pack costs as it grows, and its link degree
- `cargo run --release -p packset-daemon --example locomo -- locomo10.json`
  — retrieval quality against LoCoMo's labelled evidence
- `packset ensure` / `packset status`

## Building without system OpenSSL

`hf-hub`/`reqwest` need OpenSSL headers. Without root, build OpenSSL
once to a prefix and point the build at it; clang also needs gcc's
library dir for `-lstdc++`:

```sh
./Configure --prefix=$HOME/openssl no-docs no-apps && make -j$(nproc) build_libs && make install_sw
export OPENSSL_DIR=$HOME/openssl
export LIBRARY_PATH=$(dirname "$(gcc -print-file-name=libstdc++.so)")
cargo test --locked
```

The search binary is built on the remote builder. `just milli`
refuses anywhere else. Search falls back to the linear scorer
when the binary is absent. `PACKSET_RERANK=1` (or `/v1/search?rerank=1`)
runs the measured cross-encoder over the top 20; off by default.

## The encoder

`just embed` links ONNX Runtime from the system. The default build does
not download a runtime. Compile 1.28.0 (CPU, shared library) with
`crates/packset-embed/build-onnxruntime.sh`, then:

```sh
export ORT_LIB_PATH=$HOME/onnxruntime/lib
export ORT_PREFER_DYNAMIC_LINK=1
export LD_LIBRARY_PATH=$HOME/onnxruntime/lib
cargo build --release -p packset-embed
```

`ORT_LIB_LOCATION` is the same directory. `pkg-config` is tried first
and links a distro `libonnxruntime` of 1.24 or newer; `ORT_LIB_PATH` is
used when that probe does not succeed.
`--features download-binaries` is the cdn.pyke.io runtime.
`--features load-dynamic` reads `ORT_DYLIB_PATH` at start.
`PACKSET_EMBED_MODEL_PATH`, or `user/<model>/` under the cache, is a hub
checkout (`onnx/model.onnx` and the four tokenizer files). When those
files are present the model is not fetched. `HF_HUB_OFFLINE=1` refuses
the fetch. An absent encoder leaves search, island and recall on the
lexical ballots; `ljos doctor` reads `embedder.available: false`.

## Architecture

- `crates/packset-daemon` — the writer; atoms in LMDB, cards on disk
- `crates/packset-cli` — `packset`: lifecycle and the `/v1` reads a
  seat runs from a shell. `bin/packset` execs whichever build exists.
- `crates/packset-core` — schema, prose, recall, scoring, BM25, Borda,
  MMR, decay, extract filters
- `crates/packset-client` — HTTP
- `crates/packset-milli` — search projection
- `crates/packset-embed` — dense projection, a kept child process so the
  model loads once rather than per question

Listen on `127.0.0.1` only. Never `localhost`.

Atoms are one claim. `Remember:` / `Prefer:` are instant.
Tool dumps are attach, not atoms.

A `deed-` or `sha256:` entity is a deedar accession. The pack checks
the shape; `deedar evidence` / `deedar current` answer the rest.
The tracker cites the same accession. Completing a claimdag node
does not close a ticket.

`crates/packset-core/tests/goldens.json` is a frozen corpus, not
generated output. It fixes the accept/reject boundary and the exact error
strings, so a change to it is a change in what the daemon accepts.
