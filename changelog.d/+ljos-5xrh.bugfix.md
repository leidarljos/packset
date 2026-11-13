A search no longer keeps a copy of the pack.
The first search parsed every record, embeddings included, and held that tree until the floats were lifted out.
On a pack of 322332 KiB the search peaked at 1742296 KiB and left anonymous memory 637020 KiB above its base. A start over that pack peaked 10044 KiB above its base.
The scan now reads a step at a time, skips building the embedding array, and drops those pages before the next step. The floats stay with the records they were read with, and the stored JSON numbers are read for the one atom an answer is about.
On the same pack a start then grew 4384 KiB and the search peaked 231080 KiB, of which 219800 KiB stayed anonymous. The data file stayed a shared map.
