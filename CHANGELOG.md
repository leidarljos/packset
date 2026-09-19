# Changelog

Versions follow semver at 0.x: a minor bump is a feature, a patch is a fix.

## Unreleased

## 0.9.20 (2026-09-20)

- A persona's lens over the shared graph: `GET /v1/activate` and `POST
  /v1/fire` take `as=NAME`, the weights a persona fires are written under
  `link_weights_by[NAME]` and read back only through that lens. Nodes and
  links stay the pack's; several personas walking one island each
  tighten the paths they walked and the seat's graph moves only when the
  seat fires. `PacksetClient::activate_as`, `fire_as`, `atoms_in_set`.

## 0.9.19 (2026-09-20)

- The cue that fired an island holds it for the hour as the claims do:
  a closing that remembered a lesson first grew the island by one and
  the next closing fired a new set for the same title. Eight closings on
  one title now fire once; seven read `held`.

## 0.9.18 (2026-09-20)

- An island fires once an hour: a second fire of the same claims within
  the hour is held and says so (`held: true` on `/v1/fire` and
  `/v1/activate`). Several seats, or several personas, closing sittings
  on one issue tighten its links one step, not one step each.

## 0.9.17 (2026-09-19)

- `GET /v1/atoms?kind=persona` answers one kind; `PacksetClient::
  atoms_of_kind` asks for it. A roster of a dozen personas no longer
  reads every lesson's embedding.
- Status counts the forgotten by reason (`forgotten_by_reason`: the
  live cap, neglect, or the reason a forget gave).
- The hammer's herd arm (`HAMMER_SHARED=1`) writes every client into one
  workspace, each stamping its seat, with token-distinct claims, and ends
  by counting live plus closed against the writes: 800, 3200 and 6400
  written, none lost, on the build host at 4, 16 and 32 clients.
- Under a herd the overlap candidates come from the shortest posting
  lists (a claim carrying six tenths of the tokens sits in one of them),
  and the search index is rebuilt once per stale set under a lock and
  cached under the set's own generation; readers that arrive during a
  build wait for it, and a build finished after a further write is kept
  and patched from. Shared-workspace throughput at 16 and 32 clients
  went from 64 and 49 requests a second to 91 and 66 on a loaded node.

## 0.9.16 (2026-09-19)

- The write path stops reading the pack per write. The search index is
  projected in batches: a write queues its documents and returns, one
  indexer run lands the batch when the burst pauses (200 ms quiet, at
  most three seconds or 512 documents), and a read of the index flushes
  the queue first so a search sees its own writes. The replacement rule
  compares against the claims sharing a token, a head word or an entity
  with the new claim, read from postings over the shaped pack, not
  against every live claim: a candidate carries six tenths of the new
  claim's tokens (the overlap rule's jaccard 0.6), or opens with the
  same three words (the head rule), or shares an entity (the correction
  rule), or is named in `supersedes`; the duplicate check runs over the
  same candidates. The index cache keys on ids and stamps so the live
  set stays unshared between writes and is patched in place; a patch
  re-derives links only when an id departs or an arrival is one a cut
  link named. The pack is shaped once per process and each write shapes
  what it wrote. A fill of 10000 claims on the build host went from
  1224 s to 236 s, the hook on a prompt at 10000 from 0.18 s to 0.12 s
  cold and 0.07 s warm.
- `PACKSET_TRACE_WRITES=FILE` writes one stage-clock line per write
  (prepare, snapshot, dedup, replace, links, upsert, project, cap, in
  milliseconds, with the live count), which is how the growth above was
  found and is what to read when a fill slows with the pack.

## 0.9.15 (2026-09-19)

- An entity `seat:<name>` names the seat that wrote a claim and is not a
  topic: it links and matches nothing. Every write carries one, so a
  pack written by one seat had every claim sharing an entity with every
  other and the link step read the whole pack on each write. The peers
  that can qualify are handed to the link step borrowed, not cloned.
- The inverted index follows a write in place: a record rewritten at its
  position is re-indexed under its ordinal, an appended record is
  pushed, and only a set whose order moved is rebuilt. The first hook
  after a write no longer tokenises and indexes the whole pack.

## 0.9.14 (2026-09-19)

- Neglect reads the recalls a claim ever had, a counter no lapse resets,
  where it read the run since the last lapse; a lesson recalled and then
  missed is lapsed, not forgotten. A review block written before the
  counter reads its `reps`.
- A tombstone is never in the live set, whatever review clock it kept
  from before it was forgotten; a forgotten claim is not recalled as due.
- A write folds itself into the shown live set in place instead of
  cloning every live record and refiltering every link; a fill of 10000
  claims was quadratic. A link cut because its target was absent returns
  when the target arrives.
- Forgetting by neglect: a review left due past twice its interval is
  lapsed as a missed review would be, its stability halved and the miss
  counted; a never-recalled forgettable claim missed three times is
  tombstoned, `forgotten: neglect`. Preferences, rules, readings, goals,
  trust rows and personas lapse but are never forgotten this way. The
  writer sweeps a workspace on the first write of a day; `POST
  /v1/sweep` and `packset sweep` run it on demand.
- The writer keeps one shape per claim, the tokens, head words and
  entities the replacement and linking rules read, and linking reads only
  the peers that share an entity with the new claim, closed under their
  links. A write no longer tokenises the pack.

## 0.9.13 (2026-09-19)

- Recall: with a cue in hand the due queue takes at most a quarter of the
  budget and only claims that touch the cue; a hint matches an atom on
  half its tokens rather than one; within a tier retrievability comes
  before the write time. A pack a herd leaves with hundreds of due claims
  no longer answers every cue with them.
- `examples/forgetting.rs` ranks LRU and the recall path's due-first
  order beside the decay slots and writes the table as JSON with
  `FORGETTING_JSON`; the keep-testing order is a review order, not a
  retrieval ranking (kept claim first 0.000 against 0.947 for
  retrievability).
- `scripts/terra/forgetting.sbatch` and `scripts/terra/hammer.sbatch`.

## 0.9.12 (2026-09-19)

- A live cap: past `PACKSET_LIVE_CAP` live claims in a workspace (twenty
  thousand when unset, `off` for none) the write that crossed it forgets
  the least retrievable forgettable claims, tombstoned with
  `forgotten: the live cap`; preferences, rules, readings, goals, trust
  rows and personas are never forgotten this way. The write's answer
  says `forgot: N`; `/v1/status` says `live_cap`. A herd of seats writing
  into one pack cannot grow it without bound.
- Search hits carry the atom's `entities`, so a reader can see which seat
  wrote a claim (`seat:<name>`), which persona holds it, which habit it
  reads.

## 0.9.11 (2026-09-19)

- Many seats may run `packset ensure` at once: a spawned writer that
  exits on the port or the store lock waits for the sibling's writer
  within the startup budget and reports it, and `ensure` waits for
  `/health` to answer as packsetd before printing the URL.
- Clippy with warnings denied is clean again: the stated MSRV is the
  one the code needs (1.88), two unused encoder helpers are gone, one
  test attribute on the workers test.
- `scripts/terra/stack.sbatch` builds the pack's binaries on the build
  host.
- `cargo install packset` installs `packset` and `packsetd`.
- `packset-mcp` loads `~/.config/ljos/env` and uses that
  `PACKSET_WORKSPACE`, else `seat`. It no longer defaults to `default`,
  which is how its search missed the workspace `ljos` remembers into.

## 0.9.9 (2026-09-15)

- Concurrent encodes share one forward pass: waiting query lines are
  batched to the single embed child.

## 0.9.8 (2026-09-15)

- One `packset-embed` child encodes query and document. The prefix is
  per line. Concurrent HTTP searches share that child. Extra models
  still cost `PACKSET_EMBED_QUERY_WORKERS`.

## 0.9.7 (2026-09-15)

- packsetd no longer warms a pool of encoder children at start. Default
  query workers is 1. HTTP workers default to 4 (cap 8), not core count.
  Status counts atoms without collecting them.
- `paper_a_live` example: write-policy table on published LongMemEval_S
  and MemoryAgentBench json. Measured, not fixture. Live rows in
  =docs/orgmode/results/paper-a-live.org=.

## 0.9.6 (2026-09-15)

- Paper A fixture table: LongMemEval_S, MemoryAgentBench, MemConflict.
  Writes go through `admit_seat_write`. The table is a fixture, not SOTA.
- `admit_seat_write`: Remember / Prefer / Accept only. Raw context
  refuses. MemoryAgentBench write-policy gate.
- `ProtocolReport` names the four competencies and prints `—` when a
  split was not run. Refusal is a row, not a zero hit rate.

## 0.9.5 (2026-09-15)

- Bare `packset search` and `atoms` load `~/.config/ljos/env` so they
  speak the same workspace `ljos doctor` prints. `status` counts that
  workspace. `status --all` is the old global count.

## 0.9.4 (2026-09-15)

- Search no longer prepends the review clock. Due personas were still
  occupying half of every result list. `ljos due` is the clock.

## 0.9.3 (2026-09-15)

- A due atom with no query overlap is on the clock, not in search.
  Due personas were filling every query at score 3.1.

## 0.9.2 (2026-09-14)

- `packset-mcp --version` prints and exits so `ljos doctor` can
  version it with the rest of the set.

## 0.9.1 (2026-09-14)

- The GitHub release tarball includes `packset-embed`. Without it the
  writer reports `embedder.available: false` and search stays lexical.

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
