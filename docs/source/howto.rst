Each section is one task. Every command finds the writer on
``127.0.0.1:8761`` with nothing set; ``PACKSET_URL`` points it elsewhere and
``packset ensure`` prints the address it started.

Name the workspace once
=======================

A workspace is the git remote of the directory you stand in, or ``default``.
Set ``PACKSET_WORKSPACE`` to pin one for a shell:

.. code:: console

   $ export PACKSET_WORKSPACE=seat
   $ packset status

Every verb also takes the workspace as its last argument, and the writers
take ``--workspace WS``.

Pin a set
=========

A set is a named slice of a workspace with its own cards. Pinning one scopes
reads and deduplication to it.

.. code:: console

   $ packset pin review
   {"instructions":"","set":"review","workspace":"demo"}
   $ packset pin
   {"instructions":"","set":"review","workspace":"demo"}

The binary prints a JSON object with ``workspace``, ``set``, and ``instructions``.

Ask what was live on a date
===========================

Search answers live-now. A dated question reads the store, where a closed
claim still sits:

.. code:: console

   $ packset atoms --as-of 2026-08-01T00:00:00Z seat

``GET /v1/search`` with ``as_of`` runs the same window over a ranked answer.

Retire a claim
==============

The pack tombstones; it does not erase. The record stays readable and stops
being recalled.

.. code:: console

   $ curl -s $PACKSET_URL/v1/atoms/delete -d '{"workspace":"seat","id":"3f9c...","why":"deed-review-2026-09"}'

``why`` names the deed that showed the claim wrong. The seat's ``ljos forget``
does this with the same field.

Follow the citations
====================

A claim that names a deed accession (``deed-...`` or ``sha256:...``) is joined
to the deed store by that id and nothing else.

.. code:: console

   $ packset accessions seat | deedar evidence -    # the bytes are intact
   $ packset accessions seat | deedar current -     # still the tip
   $ packset citers deed-file-note seat             # the claims standing on one deed

Choose the panel
================

The writer fuses its ballots with a named panel. Set the three slots in the
writer's environment, not in a client:

===================== ================================================================================================================== ===========
Variable              Values                                                                                                             Default
===================== ================================================================================================================== ===========
``PACKSET_FUSE``      ``combmnz``, ``combsum``, ``rrf``, ``borda``, ``dowdall``, ``copeland``, ``schulze``, ``ranked-pairs``, ``kemeny`` ``combmnz``
``PACKSET_DIVERSIFY`` ``mmr``, ``dpp``, ``none``                                                                                         ``mmr``
``PACKSET_DECAY``     ``off``, ``on``, ``fsrs``                                                                                          ``fsrs``
===================== ================================================================================================================== ===========

``fsrs`` scales a score by the claim's retrievability from its review clock;
``on`` is a fourteen-day half-life on age. :doc:`Explanation <explanation>`
says what each was measured against.

See what a task involves
========================

Islands are the natural clusters of the link graph; a cue activates one.
On a seat's own pack of thirty claims:

.. code:: text

   $ packset islands
   9  The org to rst exporter fails with stringp nil on a src block with no language. Every begin_sr
   7  Trust rows live in the pack as trust atoms and reach the settle through ljos consensus. One le
   1  ljos pins packset-client to one git commit in its lock file. A client change needs cargo updat
   $ packset island settle a vote with learned trust
   1.000  seed  1bd8ee9e370dc5d1b6eb86aca804b15a  Trust rows live in the pack as trust atoms and reach
   0.888  seed  e7574b841812abff43b3c83e6a704d5e  A trust graph with rows into only some voters makes 
   0.810  seed  a34d89130f030ea528b5d70a3d0a22c9  The tracker verb takes --trust JSON rows laid over the con
   0.732  seed  5a779f5aee06da68e95edc90c1bf3cf6  Trust rows over static trust config for the seat's c

The first column is activation relative to the strongest; ``seed`` marks a
claim search found itself, the rest were reached along links. Add ``--fire``
when the seat goes on to use the island: the strongest eight fire together
and their links gain weight, so the next such cue walks a heavier path.

.. code:: console

   $ packset island --fire settle a vote with learned trust
   $ packset fire 1bd8ee9e... e7574b84...     # or name the claims yourself

Turn on the dense scorer
========================

The dense ballot needs the ``packset-embed`` binary beside ``packsetd`` or on
``PATH``, and a model it can load. Models are cached under
``$XDG_CACHE_HOME/packset/embed`` unless ``PACKSET_EMBED_CACHE`` names another
directory. Without the binary the pack answers from the lexical ballots and
says so in ``/v1/status``.

.. code:: console

   $ cargo binstall packset-embed
   $ packset stop && packset ensure
   $ packset status seat | jq .dense

Run the second stage on one question
====================================

The cross-encoder rerank is measured and off by default. Ask for it per
query, or set ``PACKSET_RERANK=1`` on the writer:

.. code:: console

   $ curl -s "$PACKSET_URL/v1/search?workspace=seat&q=fusion&rerank=1" | jq '.rerank'

Export for a handover, import from one
======================================

.. code:: console

   $ packset export --into bag/data/atoms seat | deedar export --into bag/data/deeds -

Import is one POST per line of the ``.jsonl``; the seat's ``ljos receive``
with ``--import`` does that after checking the bag. Trust rows travel the same way.

Serve many agents
=================

One writer serves every client on the seat. The worker pool is the balance:
``PACKSET_WORKERS`` sets it (default: the core count), a burst waits in the
accept queue rather than becoming threads, and one logical write runs at a
time so two identical claims arriving together are stored once. Reads share
one parsed snapshot per write. The encoder runs before the write lock and is
warmed at start.

.. code:: console

   $ PACKSET_WORKERS=8 packsetd --port 8761
   $ PACKSET_URL=http://127.0.0.1:8761 cargo run --release -p packset-daemon --example hammer -- 32 100

The example prints requests per second and latency percentiles for
remember and search under 32 clients; the README carries the measured
table. A second host is not a second writer: it is a client over the
network, or its own pack.

Run the retrieval benchmark
===========================

.. code:: console

   $ curl -sSLO https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json
   $ cargo run --release -p packset-daemon --example locomo -- locomo10.json

``PACKSET_LOCOMO_CONVERSATIONS=3`` scores three conversations for a quick
run; the numbers are then comparable to each other and not to a full run.
