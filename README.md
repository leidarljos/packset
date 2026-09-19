# packset

What does this seat know, standing? A pack of written claims with a review
clock: one claim per atom, written on purpose, never mined from a
transcript; ranked by a measured panel of scorers; reviewed before it is
forgotten; faded when it is not; retracted with the deed that showed it
wrong. One daemon owns one LMDB file and every tool is a client.

Docs: https://leidarljos.github.io/packset/

| Page | What it answers |
|---|---|
| [Getting started](https://leidarljos.github.io/packset/getting-started.html) | A scratch pack that remembers two claims and ranks them |
| [How-to](https://leidarljos.github.io/packset/howto.html) | Review, island, handover, one writer |
| [Reference](https://leidarljos.github.io/packset/reference.html) | Verbs, atoms, the daemon |
| [Explanation](https://leidarljos.github.io/packset/explanation.html) | Why write on purpose, and what was measured |

The seat that sits on this pack is documented at https://leidarljos.github.io.

## Install

```console
$ cargo binstall packset packset-daemon
# or: cargo install packset packset-daemon
$ packset ensure
PACKSET_URL=http://127.0.0.1:8761
$ export PACKSET_URL=http://127.0.0.1:8761
```

`packset-mcp` is the read-only MCP surface and `packset-embed` the optional
dense encoder; the pack answers without either.

## First minute

```console
$ packset remember "BM25+ is the default lexical scorer. It beat BM25 by two points."
3f9c...	lesson	due 2026-09-13T08:54:37.394Z
$ packset search lexical scorer
7.0000	lesson	3f9c...	BM25+ is the default lexical scorer. It beat BM25 by two points.
$ packset due            # tomorrow: what to review
$ packset island fusing two ballots     # the memories a task activates
```

## What holds

- Writes are `remember` and `prefer`, two sentences at most, stored as given. Nothing is extracted from a transcript.
- Every claim has a validity window and a review clock (FSRS). Retrievability scales search; on a longitudinal corpus it ranks a recalled claim first 0.947 of the time against 0.230 for words alone.
- A later claim closes the earlier one it rewrites; the closed one keeps its window for an as-of read. `consolidate` runs the rule over what is held.
- Search fuses a prefix scan, BM25+ and a dense ballot. LongMemEval_S sessions: 0.889 hit@1; MemoryAgentBench accurate retrieval 0.675 and conflict resolution 0.579 with a 7B reader.
- A workspace holds at most `PACKSET_LIVE_CAP` live claims (twenty thousand); past it the least retrievable lessons are forgotten as tombstones. Every claim carries the seat that wrote it.
- Forgetting by neglect: a review left due past twice its interval lapses as a missed review would, and a never-recalled lesson missed three times is forgotten. The writer sweeps once a day; `packset sweep` runs it now.
- Claims link by shared names; links carry weights that use strengthens; `island` returns the cluster a task activates.
- One writer, one LMDB file. Zero errors at 32 clients; throughput peaks at four. A second host is a second pack; a signed handover crosses.
- Trust rows and personas live in the pack and reach the seat's consensus.

The numbers, their jobs and how to regenerate them are on the [explanation page](https://leidarljos.github.io/packset/explanation.html) and in the [bench package](https://github.com/leidarljos/bench).

## Crates

| Crate | Carries |
|---|---|
| `packset-core` | the atom, the prose limit, BM25+, the panel, decay, islands, the review clock |
| `packset-daemon` | `packsetd`: LMDB store, HTTP, the encoder child |
| `packset-client` | the HTTP client every tool uses |
| `packset-cli` | `packset` |
| `packset-mcp` | the read-only MCP surface |
| `packset-embed` | dense, late, sparse and cross-encoder models as one child |

The retrieval, forgetting, islands, trust, many-clients and benchmark
sections that used to sit here are on the site's explanation page, with
every arm that lost.

## License

MIT.
