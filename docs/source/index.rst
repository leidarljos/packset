.. raw:: html

   <div class="vi-hero">
     <div class="vi-hero-brand">
       <img class="vi-hero-mark" src="_static/mark.svg" width="64" height="64" alt="" />
       <div>
         <p class="vi-hero-name">packset</p>
         <p class="vi-hero-tag">What does this seat know, standing?</p>
       </div>
     </div>
     <p class="vi-hero-tagline">A pack of written claims with a review clock, not a transcript with an index.</p>
     <div class="vi-hero-pills">
       <span>Remember / Prefer only</span>
       <span>LMDB, one writer</span>
       <span>CLI + HTTP + MCP</span>
     </div>
     <div class="vi-hero-actions">
       <a class="vi-btn vi-btn-gold" href="getting-started.html">Get started</a>
       <a class="vi-btn vi-btn-ghost" href="reference.html">Reference</a>
     </div>
   </div>

A pack is what a seat has decided to keep: one claim per atom, written on
purpose, never mined from a transcript. Search ranks the claims by a
measured panel of scorers. A review clock brings each claim back before it
is forgotten, and a claim that is not reviewed decays in the ranking. The
writer is one daemon over one Lightning Memory-Mapped Database (LMDB) file,
and every other tool is a client.

Install
=======

.. code:: console

   $ cargo binstall packset packset-daemon
   $ cargo binstall packset-mcp   # optional
   $ packset ensure
   PACKSET_URL=http://127.0.0.1:8761

``packset ensure`` starts the writer when it is down and prints the URL every
client reads from ``PACKSET_URL``. The dense scorer is a separate binary,
``packset-embed``, and the pack answers without it.

First minute
============

.. code:: console

   $ export PACKSET_URL=http://127.0.0.1:8761
   $ packset remember --workspace demo "BM25+ is the default lexical scorer. It beat BM25 by two points."
   3f9c... lesson  due 2026-09-13T08:54:37.394Z
   $ packset search --workspace demo lexical scorer
   9.1000  lesson  3f9c... BM25+ is the default lexical scorer. It beat BM25 by two points.

The :doc:`tutorial <getting-started>` does the same through the seat's
own verbs and ends with a handover another machine can open.

Measured
========

LongMemEval\ :sub:`S`, 470 answerable questions, session-level retrieval.
BM25+ alone is 0.914 recall@5. Fusing BM25+ with a dense ballot is
0.949 recall@5 (0.889 hit@1, 0.981 recall@10). QA on the same 470
with a local 7B reader is 0.549 against a labelled-session ceiling of
0.634. Writes only on Remember: and Prefer:. The :doc:`explanation <explanation>`
is the source for those rows.

.. toctree::
   :maxdepth: 1
   :caption: Guides
   :hidden:

   getting-started
   howto
   reference
   explanation
   architecture
