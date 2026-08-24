<p align="center">
  <img src="docs/logo/icon.svg" width="120" height="120" alt="packset">
</p>

# packset

**One writer. Typed atoms. Every agent is a client.**

`packsetd` is the seat-pack daemon on `127.0.0.1:8761`. Cards
(`USER.md`, `MEMORY.md`) stay files. Atoms live in LMDB. milli is
a search projection, not a Meilisearch server. Isolated harness
homes do not get a private store.

```
packset ensure          # start packsetd; print PACKSET_URL
packset pin NAME        # scope retrieve and Remember
# In chat:  Remember: always open review links after pushing
```

`PACKSET_URL` is the contract. `INSIDE_MEMORY_URL` is an alias.
Default writer URL is `http://127.0.0.1:8761`.

```
packset-tui             # Textual DataTable (id, kind, text, ts)
packset-tui --dump      # GET /v1/atoms as TSV
packset-tui --bump ID   # POST /v1/atoms/update {workspace, id, fields:{}}
```

The interactive app opens on identity, then on `global` (or another
store with live atoms) when identity is empty. `b` bumps the selected
row's `ts`. `w` picks another workspace (type a name or choose from
`/v1/workspaces`). `[` / `]` cycle. Urgency is a column only when an
atom carries that field.

## Law

- One writer. Working tree is not the pack.
- `Remember:` / `Prefer:` are instant. One claim per atom.
- Search merge is a host voter panel. Default is Borda then MMR,
  decay off. `PACKSET_FUSE`, `PACKSET_DIVERSIFY`, and
  `PACKSET_DECAY` select the sequence. Not a client header.
- Tool dumps and fetched bodies are not atoms.

## Crates

| Crate | Role |
|---|---|
| `packset-core` | atom schema, named fuse/diversify panel, decay, extract filters |
| `packset-client` | HTTP client |
| `packset-milli` | inverted-index projection (build on the remote builder) |

The running writer is `scripts/packsetd.py` until the Rust daemon
matches `/v1`. Clients never open the LMDB.

## Clients

Harness launchers and the seated agent talk HTTP. They do not own
the store.

## Build

Do not compile on a laptop. `just milli` and `cargo test` run on
the remote builder.

## License

MIT
