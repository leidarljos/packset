A search with no encoder splits an identifier on `_`, `-`, `.` and `/`, on a letter next to a digit, and on a camelCase boundary, and it keeps the joined form.
`list scans` meets `list_scans`, `sha1` meets `SHA-1`, and `http2` meets `HTTP/2`.
A prefix is scored by how much of the token it covers, and it stays below an exact word, so four letters of a long name stay under that word at the cut.
The index is built from the atoms when the pack is searched, so an existing pack needs no rebuild.
