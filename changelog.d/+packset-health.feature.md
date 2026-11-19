`packset health` now runs one probe with a short timeout and does not start a writer; when the URL is not answering it exits nonzero and prints `writer is not answering at URL`.
