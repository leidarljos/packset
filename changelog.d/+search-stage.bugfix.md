`packset search --rerank` prints the stage: `absent`, `settled`, or `cross-encoder`.
`packset search --rank relevance` prints `rank: relevance`.
The howto curls send the pack token, so `jq` reads `.rank` and `.rerank` instead of an authorization error.
