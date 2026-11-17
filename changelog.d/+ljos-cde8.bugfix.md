A search answers from the word and BM25 ballots while the encoder loads, and while a missing search index is built.
`/health` says `packsetd warming` and names what is still loading.
`eta_s` is included when a previous encoder load timed it.
`packsetd ok` means the encoder has answered, it gave up, or this seat has no encoder.
When work is waiting and nothing is loading, `/health` says `packsetd ok, busy`.
A full queue still answers a health probe, and answers other requests 503 with `Retry-After`.
