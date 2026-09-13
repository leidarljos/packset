Everything below runs against a scratch workspace and touches nothing else.
By the end you will have a pack that remembers two claims and ranks them.
It will tell you when to review them, and export them for another seat.

1. Start the writer
===================

.. code:: console

   $ packset ensure
   PACKSET_URL=http://127.0.0.1:8761
   INSIDE_MEMORY_URL=http://127.0.0.1:8761
   $ export PACKSET_URL=http://127.0.0.1:8761
   $ packset status demo
   packset: up on 127.0.0.1:8761 (packsetd ok)
   {
     "workspace": "demo",
     "live_by_kind": {}
   }

``packset ensure`` prints two assignment lines. Do not capture it with
``$(...)``. If a writer is already on 8761, use a scratch port instead.

One process owns the store. A second ``packsetd`` on the same file refuses
to start and names the process with the lock.

2. Remember two claims
======================

An atom is one claim in at most two sentences of at most twenty-five words
each. The pack refuses anything longer, and it refuses a tool dump outright.

.. code:: console

   $ packset remember --workspace demo "The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25."
   $ packset prefer --workspace demo "CombMNZ over RRF for fusing two ballots."
   $ packset status demo
   packset: up on 127.0.0.1:8761 (packsetd ok)
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

   $ packset search --workspace demo which fusion
   9.1000  preference  3f9c... CombMNZ over RRF for fusing two ballots.

The score is the panel's, not one scorer's. Two ballots, a prefix-and-edit
scan and BM25+ over an index, are fused by CombMNZ; see
:doc:`Explanation <explanation>` for what was measured.

4. Review before you forget
===========================

Every new claim is due for review after one day. Tomorrow:

.. code:: console

   $ packset due demo
   2026-09-13T08:54:37.394Z    3f9c... CombMNZ over RRF for fusing two ballots.
   $ packset grade 3f9c... demo
   2026-09-15T08:54:37.394Z

A recalled claim comes back later; ``--lapsed`` brings it back sooner. With
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
