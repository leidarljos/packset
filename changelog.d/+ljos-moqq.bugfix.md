Commit 80e0c4e gave the large-folio start peaks as 20, 31 and 35 MiB over a 6 MiB base, for packs of 11, 47 and 95 MiB.
Those are the absolute peaks; over the base they were 14, 25 and 29 MiB.
The same commit dropped the map-growth entry, and 4fb290e restored it unchanged in its own fragment, so the entry appears once.
