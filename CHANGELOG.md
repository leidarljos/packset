# Changelog

Versions follow semver at 0.x: a minor bump is a feature, a patch is a fix.

## Unreleased

- An island seeded by hits fewer than two scorers agreed on is reported
  `weak` and is not fired: activation from weak seeds flows to the
  best-connected cluster whatever the cue, and firing it wires the wrong
  links tighter. The answer carries `agreed_seeds` and whether the dense
  ballot ran (`dense`).
- `GET /v1/islands` returns communities by modularity (the Louvain
  method) with the partition's modularity and a structural signature per
  island (Weisfeiler-Lehman refinement over the links), and reports label
  propagation's count and modularity beside them.
- `consolidate` buckets only by the entities a claim carries and its
  first words, and passes over a bucket of more than 64: the entities
  read off a text made one bucket of every claim and a pass over every
  pair, 2.7 s at a thousand claims.
- The writer reads `PACKSET_HOME` for its home, the name the pack's other
  variables share; the older names still answer. Two writers on one host
  with different homes no longer open the same store, and each writes
  its own log (`packsetd-PORT.log`).
- A network arm: with `PACKSET_MAB_GNN` naming a directory of inferred
  edges (one JSON a record, from a graph neural network trained to tell
  the record's own edges from non-edges), the second hop is the facts the
  network believes the first hop's facts link to.
- The two-hop arm keeps the first hop's five strongest facts ahead of
  the second hop's, so a single-hop question keeps its answer in front.
- A saddle arm: with `PACKSET_MAB_SURVEY` naming a directory of
  landscape surveys (one JSON a record: cores with members, saddles with
  bridges), the second hop is the facts nearest the saddles out of the
  cores the first hop's facts descended into.
- The two-hop arm measured on conflict resolution, same job and reader:
  0.576 against 0.535 for the live facts alone (+0.041, interval +0.014
  to +0.070); multi-hop rows 0.21 to 0.40 against 0.06 to 0.31; single-hop
  rows give back one to nine points to the second hop's facts.
- A two-hop arm on the MemoryAgentBench fact lists: the objects of the
  strongest live facts become second queries, and the second hop's live
  facts follow the first's, so a multi-hop question sees the bridge and
  the fact it leads to with every superseded fact already closed.

## 0.9.0 (2026-09-13)

- MemoryAgentBench accurate retrieval measured (2000 questions, ten
  chunks, 7B reader): the fused panel 0.675 of the questions, lexical
  0.674, five chunks 0.649; averaged by source 0.644 and 0.642, against
  the published BM25 0.605 and HippoRAG-v2 0.651 with a GPT-4o-mini
  reader.
- The reading lists the days between every pair of retrieved sessions,
  by default: over the same 350 questions the raw prompt answers 0.431,
  the marked distances 0.420, the listed gaps 0.460 (temporal reasoning
  0.315 to 0.386, knowledge-update 0.792 to 0.819). The store does the
  date arithmetic; the reader places the events.
- MemoryAgentBench conflict resolution measured (800 questions, ten
  facts, 7B reader): the fused panel 0.33 to 0.55 single-hop; the same
  hits with superseded facts closed by the replacement rule 0.76 to 0.86
  single-hop, 0.480 over the split; multi-hop stays under 0.3 for every
  arm, as it does for every published system.
- Test-time learning on LoCoMo measured (1540 questions, ten turns): the
  baseline 0.584; the review clock moved by the judge's verdict 0.445;
  moved by the gold evidence 0.544. Retrievability that decays every
  unreviewed turn from the start loses more than confirmed turns gain,
  on a benchmark whose every question asks about something not yet
  asked. The review clock is for what a seat returns to, not for a
  haystack read once.
- `--bench mab` reads a dump without the dataset's words: the positional
  dataset argument is the directory of split files, and questions and
  answers come back by split, row and index.

## 0.8.0 (2026-09-12)

- The timeline reading measured against the raw prompt (350 questions,
  four types, 7B reader): 0.420 against 0.431 overall, temporal reasoning
  0.339 against 0.315, multi-session 0.331 against 0.372. Within noise and
  not a lever for this reader; kept as an option.
- Test-time learning on LoCoMo: the dump carries the fused list with
  scores, and `scripts/longmemeval_qa.py --learn fsrs` answers each
  conversation's questions in order, grades the turns the reader was
  handed by the judge's verdict, and reweighs the next question's hits by
  the review clock (the FSRS update in packset-core); `--learn oracle`
  grades by the gold evidence, `--learn none` is the baseline. The review
  clock measured on public data, moved by feedback rather than labels.
- `POST /v1/consolidate`: the write-time replacement rule run over the
  live set in the order it was written, so a pack written before the
  rule, or filled by import, closes what it should have; `apply` false
  reports the pairs and writes nothing. Pairs are drawn from claims that
  open with the same three words or share an entity, so the count stays
  cheap on a large pack.
- A claim replaces an earlier one of the same kind when the two share a
  head and differ in the object (`The default fuse is Borda` to `The
  default fuse is CombMNZ`; `Roy Rogers is married to Dale Evans` to
  `... John McVie`, which a set measure missed), and a claim without
  entities is read by its text: the seat's own lessons name none, so the
  pack had never closed one. Entities on both sides still have to meet.
  The MemoryAgentBench live arm uses this rule.
- Every fused hit says how many ballots named it (`ballots`) out of how
  many ran (`of`), so a reader can keep to what the scorers agree on.
- A MemoryAgentBench harness (`examples/memoryagentbench.rs`): the
  accurate-retrieval and conflict-resolution records chunked as the
  benchmark chunks them (facts one a document), lexical, dense and fused
  arms, and for fact lists a latest-first arm and a live arm in which a
  later fact with the same head closes the earlier one, the pack's
  supersession stated as a rule. `scripts/longmemeval_qa.py --bench mab`
  reads the dump with positions marked and scores by substring match, the
  LongMemEval rows by their judge.
- The LongMemEval harness gains an island arm: sessions link to their five
  nearest by dense cosine, the fused top ten seed the writer's own
  spreading activation over that graph, and the cluster is ranked. The
  island the seat reads at a sitting, measured on the benchmark, and a
  measured negative as a ranking: hit@1 0.377 against the fused 0.889 over
  470 questions, recall@10 unchanged at 0.981. Activation follows the
  graph's degree, not the question; the island is orientation beside the
  hits, which is where the seat prints it, not a ranking in their place.

## 0.7.2 (2026-09-12)

- The lockfile follows the version bump. The release builds of v0.7.0 and
  v0.7.1 refused `--locked`: the crates' path dependencies still required
  0.6.0, so the lock could not move. They now require the workspace
  version.

## 0.7.1 (2026-09-12)

- The version bump alone; its release build was refused (see 0.7.2).

## 0.7.0 (2026-09-12)

- Search hits and island rows carry `ts`, when the memory was written, so
  a reader can lay what it recalls on a timeline; a hit from the index
  projection takes its stamp, kind and review date from the pack's record.
- LoCoMo answer accuracy, 1540 questions, 7B reader and judge, dated
  turns: labelled evidence 0.747, fused panel 0.637 at twenty turns and
  0.577 at ten, lexical 0.488 at ten. Fused over lexical by nine points at
  the same depth; depth buys most on multi-hop questions.
- The full answer-accuracy run, 470 questions, top five sessions, 7B
  reader and judge: labelled sessions 0.634, fused panel 0.549, lexical
  0.532; fused leads where retrieval decides, the reader decides the rest.
- The window arm measured: fifteen of 470 questions name a time the parser
  reads; on temporal reasoning hit@1 0.811 to 0.835, recall@5 0.899 to
  0.878, the rest unchanged.
- A window arm on LongMemEval: the time a question names (a date, a month,
  a count of units ago, last week or month) read into a window over the
  sessions and those inside scored twice, a filter the question asks for
  rather than a decay.
- The answer-accuracy script hands the reader the seat's reading of time:
  every session marked with its distance in days before the question and
  the rule that a later session supersedes an earlier one on the same fact
  (`--no-timeline` for the benchmark's raw prompt); `--types` runs a subset.

## 0.6.0 (2026-09-12)

- The writer keeps two query encoders (`PACKSET_EMBED_QUERY_WORKERS`), so
  agents asking at once are answered side by side instead of one behind
  the other; eight concurrent hooks took 281 ms wall on one encoder and
  243 ms on two once warm. The pool is warmed at start, since the first
  use of a cold second encoder cost 770 ms.
- LoCoMo at turn granularity, 1986 questions: the fused panel finds an
  evidence turn first 0.414 of the time and within ten 0.759, against
  0.318 and 0.647 for the lexical ballot.
- A recency arm on LongMemEval, measured as a negative: scaling the fused
  score by the fourteen-day temporal slot for the session's age at the
  question falls from 0.889 to 0.551 hit@1 and loses on every type but
  temporal reasoning, knowledge-update included. Forgetting by age alone
  throws away what a question needs.
- LongMemEval_S over every answerable question: the fused panel reaches
  0.889 hit@1, 0.949 recall@5, 0.981 recall@10 at session granularity,
  against 0.855, 0.914, 0.952 for the lexical ballot.
- `packset hubs` and `GET /v1/hubs`: the claims the link graph turns on,
  a weighted PageRank over the links.
- Two kinds: `prediction` (a voter's forecast on an issue, for the
  surprisingly popular rule) and `rule` (a pattern with a verdict, argv law
  kept in the pack and exported with the rest).

## 0.5.1 (2026-09-12)

- Two panel tests were red at 0.5.0: `Panel::parse` still filled the decay
  slot with `off`, and a test read the default as `off`.
- The first answer-accuracy row: with Qwen2.5-7B-Instruct Q5_K_M as
  reader and judge and LongMemEval's own prompts, the fused panel answers
  0.630 of the first hundred questions, the lexical ballot 0.570, the
  labelled sessions 0.690.
- A cross-encoder rerank arm on LongMemEval (`PACKSET_LME_RERANK=1`),
  measured and kept opt-in: bge-reranker-base over the windows of the fused
  top sessions falls from 0.920 to 0.780 hit@1 on the first hundred
  questions.
- `examples/locomo_dump` and `scripts/longmemeval_qa.py --bench locomo`:
  the same retrieval dump and answer-accuracy seam for LoCoMo.

## 0.5.0 (2026-09-12)

- LongMemEval_S with an encoder: the fused panel over session documents
  reaches 0.920 hit@1 and 0.968 recall@5 on the first hundred questions,
  against 0.840 and 0.904 for the lexical ballot alone.
- `examples/longmemeval` writes the sessions each arm retrieved to
  `PACKSET_LME_DUMP`, and `scripts/longmemeval_qa.py` turns that into
  LongMemEval answer accuracy with the benchmark's reading and judge
  prompts over any OpenAI-compatible reader and judge.
- The writer reads `PACKSET_FUSE`, `PACKSET_DIVERSIFY` and `PACKSET_DECAY`
  when the flag of the same name is absent. It read only the flags, so a
  writer started with `PACKSET_DECAY=fsrs` in its environment ran with
  decay off and `status` said so.
- The decay slot defaults to `fsrs`: retrievability from the review clock,
  floored at 0.25, cards exempt. `PACKSET_DECAY=off` turns it off. On a
  corpus with no review history the slot changes nothing.

## 0.4.0 (2026-09-12)

- `PacksetClient::with_workspace` pins the workspace ahead of
  `PACKSET_WORKSPACE` and the working directory, so a seat can be one memory
  across every repository it works in.

- The client finds the writer at `http://127.0.0.1:8761` when `PACKSET_URL`
  is unset (`PACKSET_PORT` moves the port), the same default the command
  line and the MCP server use, so a seat needs no variable set.
  `PACKSET_URL=off` is the one way to have no pack.

## 0.3.0 (2026-09-12)

What a user gets:

- A review clock on every claim: written claims are due after one day,
  `grade` moves them, `due` lists what is about to be forgotten. With
  `PACKSET_DECAY=fsrs` the FSRS retrievability curve scales search scores,
  so unreviewed claims fade without vanishing.
- Memory islands: `islands` lists the link graph's clusters, `island CUE`
  returns the memories a task activates by spreading activation, and `fire`
  (or `island --fire`) strengthens the links of claims used together, with a
  forgetting term so weights stay bounded.
- Trust rows: a `trust` atom is one weighted edge of an influence graph, with
  a validity window like any claim, exported with the rest.
- The pack's own command line: `remember`, `prefer`, `search`, `due`,
  `grade`, `islands`, `island`, `fire`, beside `ensure`, `status`, `export`.
- A hit's `score` is the panel's fused weight, one scale across ballots; the
  ballot's own score sits beside it as `ballot_score`.
- BM25+ replaces BM25 as the lexical default; passage windows measured; the
  retrieval table names every arm and what lost.
- LongMemEval_S session retrieval and a many-clients hammer are examples
  with their tables in the README.
- One logical write at a time under many clients; the encoder runs before
  the write lock and is warmed at start; the index is rebuilt from cached
  tokens after a write.
- `packset-embed` caches models under `$XDG_CACHE_HOME/packset/embed`, never
  the working directory.
- A sentence boundary needs whitespace after the full stop, so `0.9.3`,
  `127.0.0.1` and `Cargo.lock` no longer count as sentence ends.
- Client requests time out after thirty seconds, settable with
  `PACKSET_TIMEOUT_MS`; refusals carry the daemon's one-line reason.
- A documentation site at https://leidarljos.github.io/packset/.

## 0.2.0

The Rust writer: LMDB store, HTTP surface, the panel of fusers and
diversifiers, the dense encoder as a child process, export for handovers.
