A reader that had already seen a live snapshot could then be shown a shorter one.
The snapshot was rebuilt from the database in the gap after a write committed and before that write was published, so the rebuild contained a record the published snapshot did not. The following read served the published snapshot, and the count went backwards.
A write now publishes the commit and the snapshot before any rebuild can see that commit, and a rebuild that finds a snapshot already published for that generation keeps it. Pages of the database file are dropped only while no reader is in the map.
