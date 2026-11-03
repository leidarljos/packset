#!/bin/sh
# Run one cargo test command n times with TMPDIR set, and keep each run's whole output, after a header that says where and on what it ran.
# Usage: measurements/start-moqq/run.sh <log prefix> <n> <tmpdir> <store commit> <cargo test args>...
set -u
prefix=$1
n=$2
tmpdir=$3
store=$4
shift 4
mkdir -p "$(dirname "$prefix")"
cargo test "$@" --no-run >/dev/null 2>&1
i=1
while [ "$i" -le "$n" ]; do
    log=$prefix-$i.log
    {
        echo "# store code at $store, with the start test of measurements/start-moqq"
        echo "# Linux $(uname -r), $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'), $(nproc) CPUs"
        if sudo -n true 2>/dev/null; then echo "# root through sudo: yes"; else echo "# root through sudo: no"; fi
        echo "# TMPDIR=$tmpdir on $(findmnt -no FSTYPE,OPTIONS -T "$tmpdir")"
        echo "# cargo test $*"
        TMPDIR=$tmpdir cargo test "$@" 2>&1 </dev/null
        echo "# exit $?"
    } >"$log"
    i=$((i + 1))
done
