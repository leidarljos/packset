#!/bin/sh
# Run every condition in a list n times, a draw of each before the next draw of any, and keep each run's whole output after a header that says when, where and on what it ran.
# Odd draws take the list in order and even draws in reverse (ABBA).
# Each draw's header gives its start time, and a drift over the session falls on each condition alike.
# Usage: `measurements/start-moqq/run.sh <conditions file> <n>`.
# Each line of that file is fields split by |: `<log prefix>|<worktree>|<tmpdir>|<store code>|<PACKSET_START_SIZES, or ->|<cargo test args>`.
# A log prefix is relative to the directory of the conditions file, and each worktree builds into its own target directory.
set -u
set -f
list=$1
n=$2
logs=$(cd "$(dirname "$list")" && pwd)
count=$(grep -c . "$list")
grep . "$list" | while IFS='|' read -r prefix worktree tmpdir store sizes args; do
    (cd "$worktree" && CARGO_TARGET_DIR=$worktree/target cargo test $args --no-run >/dev/null 2>&1 </dev/null)
done
i=1
while [ "$i" -le "$n" ]; do
    if [ $((i % 2)) -eq 1 ]; then order=cat; else order=tac; fi
    position=1
    grep . "$list" | $order | while IFS='|' read -r prefix worktree tmpdir store sizes args; do
        log=$logs/$prefix-$i.log
        mkdir -p "$(dirname "$log")"
        {
            echo "# draw $i of $n, $position of $count in it, started $(date -u +%Y-%m-%dT%H:%M:%S.%NZ)"
            echo "# store code at $store, with the start test of measurements/start-moqq"
            echo "# Linux $(uname -r), $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'), $(nproc) CPUs"
            if sudo -n true 2>/dev/null; then echo "# root through sudo: yes"; else echo "# root through sudo: no"; fi
            echo "# TMPDIR=$tmpdir on $(findmnt -no FSTYPE,OPTIONS -T "$tmpdir")"
            if [ "$sizes" = - ]; then
                echo "# cargo test $args"
                (cd "$worktree" && TMPDIR=$tmpdir CARGO_TARGET_DIR=$worktree/target cargo test $args 2>&1 </dev/null)
            else
                echo "# PACKSET_START_SIZES=$sizes cargo test $args"
                (cd "$worktree" && PACKSET_START_SIZES=$sizes TMPDIR=$tmpdir CARGO_TARGET_DIR=$worktree/target cargo test $args 2>&1 </dev/null)
            fi
            echo "# exit $?"
        } >"$log"
        position=$((position + 1))
    done
    i=$((i + 1))
done
