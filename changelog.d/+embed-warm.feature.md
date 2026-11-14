`packsetd` starts the query encoder when it begins listening, so the first search is not the one that loads the model.
`/health` does not wait on that load.
While it runs, `/v1/status` reports `embedder.remaining_ms`: the last cold start written to `load-ms` in the embed cache, minus the time already spent, or one second when that file is absent.
The one second is a ceiling, not a measurement of this seat. The next cold start that answers replaces it.
The field is left out once the encoder is answering, and when it is unavailable.
A missing binary starts nothing. A binary that does not answer is logged once, and search stays lexical.
