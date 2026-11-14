.. title:: Getting started

:description: Remember two claims in a scratch workspace, rank them, review them, and export the pack.

Everything below runs against a scratch workspace and touches nothing else.
By the end you will have a pack that remembers two claims and ranks them.
It will tell you when to review them, and export them for another seat.

Install the writer. The encoder is a separate binary. Without it search
stays lexical, and a sitting still opens on those ballots.
:doc:`How-to <howto>` covers building the encoder from source against
an Open Neural Network Exchange (ONNX) Runtime you compile.

.. code:: console

   $ cargo install --locked packset

``cargo binstall --locked packset`` is the same install when ``cargo-binstall``
is already on the machine. Both reach crates.io, so they need a network.
A checkout of this repository builds with ``cargo build --release -p packset``,
and ``bin/packset`` runs that binary.

1. Start the writer
===================

.. code:: console

   $ packset ensure
   packset: listening on 127.0.0.1:8761
   PACKSET_URL=http://127.0.0.1:8761
   INSIDE_MEMORY_URL=http://127.0.0.1:8761
   $ export PACKSET_URL=http://127.0.0.1:8761
   $ packset status demo
   packset: up on http://127.0.0.1:8761 (packsetd ok)

``packset ensure`` prints two assignment lines, and ``listening`` when it
started the writer. Do not capture it with ``$(...)``. If a writer is already
on 8761, the error names that process and a free port:

.. code:: text

   port 8761 is held by pid 1234 (packsetd) for another pack; this client uses /tmp/other-pack, and the writer refused its token (401). A second pack needs its own port: PACKSET_PORT=8762 packset ensure

Run that command. It starts this pack beside the one already listening.
``packset stop`` stops the writer on this shell's port, so with the port
left at 8761 it stops the other pack.

The status line is followed by one JSON object. Read ``live_by_kind``.
The object also names ``home``, ``version`` and ``commit``. A writer whose
version or commit is not this client's names ``packset stop && packset ensure``.

One process owns the store. A second ``packsetd`` on the same files names
the process that holds them, and the command for a second pack, which is
another home and another port:
``PACKSET_HOME=/tmp/other-pack PACKSET_PORT=8762 packset ensure``.

2. Remember two claims
======================

An atom is one claim. Two sentences of at most twenty-nine words each read
best. A longer claim is stored with the reason in ``prose.warning``. A tool
dump is stored with the label ``flag:tool-dump``.

.. code:: console

   $ packset remember --workspace demo "The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25."
   $ packset prefer --workspace demo "CombMNZ over RRF for fusing two ballots."
   $ packset status demo
   packset: up on http://127.0.0.1:8761 (packsetd ok)
   {
     "live_by_kind": {
       "lesson": 1,
       "preference": 1
     }
   }

Sending the same lesson twice returns the stored atom. Sending a contrary
lesson closes the old one's validity window.

3. Ask what the seat knows
==========================

.. code:: console

   $ packset search --workspace demo ballots BM25
   packset: workspace demo
   2.0000  preference  63df91bbea2b5e4e146866db0a85b4c6  CombMNZ over RRF for fusing two ballots.
   1.0000  lesson  ee17d2a07cbf1569e54e0d80496fdecf  The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25.

The id is the one ``prefer`` or ``remember`` printed. Right after those two
writes the scores are 2.0000 and 1.0000; the number moves as the review
clock runs. A question that shares no words prints ``no claims matched``
and the workspace. When no writer is listening, search returns at once
with ``writer is not running on 127.0.0.1:8761; packset ensure starts it``.
``packset stop`` with nothing listening says ``nothing of ours to stop``
at once, and it does not start a writer.

The score is the panel's, not one scorer's. Two ballots, a prefix-and-edit
scan and BM25+ over an index, are fused by CombMNZ; see
:doc:`Explanation <explanation>` for what was measured.

4. Review before you forget
===========================

Every new claim is due for review after one day. The same day,
``packset due demo`` prints ``nothing is due in demo`` and the next review.
Tomorrow:

.. code:: console

   $ packset due demo
   2026-09-13T08:54:37.394Z    3f9c... CombMNZ over RRF for fusing two ballots.
   $ packset grade 3f9c... demo
   2026-09-15T08:54:37.394Z

A recalled claim comes back later; ``--lapsed`` brings it back sooner.
By default a claim you have not reviewed also sinks
in the ranking as its retrievability falls.

5. Hand the pack over
=====================

.. code:: console

   $ packset export --into /tmp/bag/data/atoms demo
   2 atoms to /tmp/bag/data/atoms/demo.jsonl

The file is one JSON object a line, the same shape as the store's records. Every
deed accession the atoms cite is printed on stdout, so a deed store can be
asked for the products in the same pipe:

.. code:: console

   $ packset export --into /tmp/bag/data/atoms demo | deedar export --into /tmp/bag/data/deeds -

The seat's ``ljos handover`` runs that pipe, seals the bag, and signs it.

Where next
==========

-  :doc:`How-to <howto>`: pin a set, ask what was live on a date, turn on the dense scorer, choose a panel.
-  :doc:`Reference <reference>`: every verb, endpoint, environment variable, and field.
-  :doc:`Explanation <explanation>`: why a pack is not a transcript, and what the retrieval table says.
