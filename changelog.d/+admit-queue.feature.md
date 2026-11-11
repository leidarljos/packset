Reads, writes and maintenance wait in their own queues, and a read is served first.
One workspace cannot take every turn.
A full queue, or Linux memory pressure, answers 503 with `Retry-After` before the request runs.
Writes are turned away before reads, and maintenance pauses first.
The client sends `X-Packset-Deadline` at the end of its budget. A request past that deadline does not run.
A retry waits at least `Retry-After`, with jitter, inside the budget it already had.
`GET /stats` reports the queue depths, the shed counts and the limits. `line` is `busy, retrying` when work is waiting.
The limits stay at the ceiling on a quiet seat. A request does not wait.
