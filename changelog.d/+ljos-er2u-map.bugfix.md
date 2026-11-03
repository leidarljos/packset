A pack no longer stops taking writes when it reaches 1 GiB.
The map now opens at twice the pack's size on disk.
It doubles when a write needs room, and that includes the upgrade pass.
