#!/bin/sh
# Check out a commit at $2 and put this tree's start tests in its store.
# The test then runs over the store as that commit had it.
# Usage: measurements/start-moqq/before.sh <commit> <dir>
set -eu
rev=$1
dir=$2
store=crates/packset-cli/src/bin/packsetd/store.rs
git worktree add --detach "$dir" "$rev"
awk '/^    \/\/\/ How many times its bound the pack in$/ { on = 1 } on && /^}$/ { exit } on' \
    "$store" >"$dir/start-tests.rs"
awk -v tests="$dir/start-tests.rs" '
    /^mod tests \{$/ { inside = 1 }
    inside && /^}$/ { while ((getline line < tests) > 0) print line; inside = 0 }
    { print }
' "$dir/$store" >"$dir/store.rs"
mv "$dir/store.rs" "$dir/$store"
rm "$dir/start-tests.rs"
