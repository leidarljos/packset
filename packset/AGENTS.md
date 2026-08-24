# AGENTS.md

The seat pack (`USER.md`, `MEMORY.md`, atoms) is not this tree.
Do not embed git-tracked files into memory.

## Commands

- `just test-py` — pytest on `scripts/test_*.py`
- `just lint` — ruff
- `packset ensure` / `packset status`

The search binary is built on the remote builder. `just milli`
refuses anywhere else. Search falls back to the linear scorer
when the binary is absent.

## Architecture

- `scripts/packsetd.py` — one writer; atoms in LMDB
- `crates/packset-core` — Borda, MMR, decay, extract filters
- `crates/packset-client` — HTTP
- `crates/packset-milli` — search projection

Listen on `127.0.0.1` only. Never `localhost`.

Atoms are one claim. `Remember:` / `Prefer:` are instant.
Tool dumps are attach, not atoms.
