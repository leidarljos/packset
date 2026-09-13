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
$ export PACKSET_URL=$(packset ensure)
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

- Writes are `remember` and `prefer`, two sentences at most, stored as
  given. Nothing is extracted from a transcript. A retraction names the deed
  that withdrew the claim.
- Every claim has a validity window and a review clock (FSRS,
  doi:10.1145/3534678.3539081). Retrievability scales search by default
  (`PACKSET_DECAY=off` turns it off): on a longitudinal corpus where one early claim is kept
  recalled and three late paraphrases never are, it ranks the kept claim
  first 0.947 of the time against 0.230 for lexical scoring alone and 0.270
  for a recency half-life (`examples/forgetting.rs`).
- Search fuses a prefix-and-edit scan, BM25+ over an index, and a dense
  ballot when an encoder is present. Measured on LoCoMo: 0.736 hit@1 fused
  against 0.752 published; on LongMemEval_S over every answerable question
  (470), 0.914 recall@5 at session level with BM25+ alone and 0.949
  recall@5 (0.889 hit@1, 0.981 recall@10) with the dense ballot fused in
  over session documents. Mem0 and Zep publish answer accuracy on the same
  benchmarks (doi:10.48550/arXiv.2504.19413, doi:10.48550/arXiv.2501.13956);
  the same metric measured here with a local reader (Qwen2.5-7B-Instruct
  Q5_K_M as reader and judge, LongMemEval's own prompts, top five sessions)
  over all 470 answerable questions: the fused panel answers 0.549, the
  lexical ballot alone 0.532, and the labelled sessions (the ceiling for
  any retriever) 0.634; the fused panel leads where retrieval decides
  (multi-session, single-session-user, preference) and the reader decides
  the rest. On LoCoMo at turn granularity, same reader and judge, 1540
  questions: the fused panel answers 0.637 with twenty turns and 0.577
  with ten, the lexical ballot 0.488 with ten, the labelled evidence turns
  0.747. `scripts/longmemeval_qa.py` runs it over the
  harness's retrieval dump with any OpenAI-compatible reader and judge; the
  explanation page says how to read the result.
- A later claim closes the earlier one it rewrites: a rewrite, a correction
  sharing an entity, or the same opening words with a new object; the
  closed one keeps its window for an as-of read. `POST /v1/consolidate`
  runs the rule over what is held and reports the pairs before writing.
  On MemoryAgentBench's conflict-resolution split the same rule takes a
  7B reader from 0.33 to 0.55 single-hop to 0.76 to 0.86, 0.480 over the
  split, where the published retrieval baselines with a stronger reader
  sit at 0.155 to 0.295. On its accurate-retrieval split the fused panel
  answers 0.675 of 2000 questions with the same 7B reader, against the
  published 0.605 (BM25) and 0.651 (HippoRAG-v2) with a hosted reader.
- Claims link by shared names; links carry weights that use strengthens and
  disuse decays; `island` returns the cluster a task activates.
- A `trust` atom is one weighted edge of an influence graph, scoped to
  domains by its entities; a `persona` atom is a voter with its own anchor.
  Both are exported with the rest and read by the seat's consensus.
- One logical write at a time; 150 to 230 requests a second at 32 clients on
  four cores, no failures.

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
