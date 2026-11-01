#!/usr/bin/env bash
# Write the commit a build should claim, for a build host that has no history.
#
# The source often reaches a build host by copy rather than by clone, so git
# there answers for whatever that host's own checkout sits at. A binary stamped
# from that carries a commit it was not built from, which is worse than no
# stamp: it is wrong with confidence.
#
# Run this before sending the tree, and have the build export
# PACKSET_COMMIT from the file:
#
#     scripts/stamp-commit.sh
#     rsync -a --exclude target --exclude .git ./ <host>:<path>/
#     # on the host, before cargo build:
#     export PACKSET_COMMIT=$(cat <path>/.build-commit)
#
# A build with no stamp and no history says "unknown", which is the honest
# answer and never reads as current.
set -euo pipefail
cd "$(dirname "$0")/.."
sha=$(git rev-parse --short=8 HEAD)
if [ -n "$(git status --porcelain)" ]; then
    sha="$sha+"
fi
printf '%s\n' "$sha" > .build-commit
printf 'stamped %s\n' "$sha"
