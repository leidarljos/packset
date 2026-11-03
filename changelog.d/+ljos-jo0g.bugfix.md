A write reads the live claims of its own set, which the writer patches as claims are added, updated and deleted.
It used to walk every live claim in the workspace under the lock, and that walk grew with the workspace.
On a pack of 20,000 live claims the steady dedup stage fell from 23 ms to 10 ms.
