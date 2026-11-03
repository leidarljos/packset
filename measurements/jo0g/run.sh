#!/bin/sh
# Time writes against a copied pack, one draw of every condition before the next draw of any.
# Odd draws take the list in order and even draws in reverse.
# Each line of the conditions file is fields split by |:
# `<log prefix>|<packsetd binary>|<seed home>|<filesystem directory>|<port>`
# The log prefix is relative to the directory of the conditions file.
# Usage: measurements/jo0g/run.sh <conditions file> <n>
set -u
set -f
list=$1
n=$2
here=$(cd "$(dirname "$0")" && pwd)
logs=$(cd "$(dirname "$list")" && pwd)
count=$(grep -c . "$list")
i=1
while [ "$i" -le "$n" ]; do
    if [ $((i % 2)) -eq 1 ]; then order=cat; else order=tac; fi
    position=1
    grep . "$list" | $order | while IFS='|' read -r prefix binary seed fs port; do
        log=$logs/$prefix-$i.log
        mkdir -p "$(dirname "$log")" "$fs"
        dest=$fs/draw-$prefix-$i
        trace=$fs/trace-$prefix-$i.log
        {
            echo "# draw $i of $n, $position of $count in it, started $(date -u +%Y-%m-%dT%H:%M:%S.%NZ)"
            echo "# packsetd $binary"
            echo "# seed $seed"
            echo "# Linux $(uname -r), $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'), $(nproc) CPUs"
            echo "# dest $dest on $(findmnt -no FSTYPE,OPTIONS -T "$fs")"
            echo "# python3 $here/drive.py probe"
            python3 "$here/drive.py" probe "$binary" "$seed" "$dest" "$trace" "$port"
            echo "# exit $?"
            echo "# trace"
            cat "$trace"
        } >"$log"
        rm -rf "$dest" "$trace"
        position=$((position + 1))
    done
    i=$((i + 1))
done
