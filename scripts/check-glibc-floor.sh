#!/bin/sh
# Fail when a shipped Linux binary needs a glibc or libstdc++ symbol
# newer than Ubuntu 22.04.
#
# The release links packset-embed on that image. A newer image records
# GLIBC and GLIBCXX versions the seat does not have, and the dynamic
# linker exits the encoder before main.
set -eu

# Ubuntu 22.04 ships glibc 2.35 and libstdc++ from GCC 12.
# Those two versions, and CXXABI_1.3.13, are the newest that library provides.
GLIBC_FLOOR=2.35
GLIBCXX_FLOOR=3.4.30
CXXABI_FLOOR=1.3.13

# yes when $1 is strictly newer than $2. Components, not a decimal,
# so 2.3.4 is older than 2.35.
newer() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, A, ".")
    m = split(b, B, ".")
    len = n
    if (m > len) len = m
    for (i = 1; i <= len; i++) {
      av = 0
      bv = 0
      if (i <= n && A[i] != "") av = A[i] + 0
      if (i <= m && B[i] != "") bv = B[i] + 0
      if (av > bv) { print "yes"; exit }
      if (av < bv) { print "no"; exit }
    }
    print "no"
  }'
}

floor_for() {
  case "$1" in
    GLIBC) printf '%s\n' "$GLIBC_FLOOR" ;;
    GLIBCXX) printf '%s\n' "$GLIBCXX_FLOOR" ;;
    CXXABI) printf '%s\n' "$CXXABI_FLOOR" ;;
    *)
      echo "no floor for $1" >&2
      return 1
      ;;
  esac
}

# The last component plus one: 2.35 -> 2.36, 3.4.30 -> 3.4.31.
bump_last() {
  printf '%s\n' "$1" | awk -F. 'BEGIN { OFS = "." } { $NF = $NF + 1; print }'
}

self_test() {
  for family in GLIBC GLIBCXX CXXABI; do
    floor=$(floor_for "$family")
    if [ "$(newer "$floor" "$floor")" != no ]; then
      echo "$family floor $floor compared newer than itself" >&2
      exit 1
    fi
    above=$(bump_last "$floor")
    if [ "$(newer "$above" "$floor")" != yes ]; then
      echo "$family $above should be newer than $floor" >&2
      exit 1
    fi
    if [ "$(newer "$floor" "$above")" != no ]; then
      echo "$family $floor should not be newer than $above" >&2
      exit 1
    fi
  done
  # A decimal compare treats 2.3.4 as 2.3 and 2.35 as greater the wrong way
  # once the extra component is dropped. The scan must not do that.
  if [ "$(newer 2.3.4 2.35)" != no ]; then
    echo "2.3.4 was treated as newer than 2.35" >&2
    exit 1
  fi
  if [ "$(newer 2.35 2.3.4)" != yes ]; then
    echo "2.35 was not newer than 2.3.4" >&2
    exit 1
  fi
  echo "symbol floor self-test passed (GLIBC $GLIBC_FLOOR, GLIBCXX $GLIBCXX_FLOOR, CXXABI $CXXABI_FLOOR)"
}

# Undefined versioned symbols the dynamic linker will demand.
# objdump prints them as `(GLIBC_2.34)` on `*UND*` rows.
needed_versions() {
  dump=$(objdump -T "$1") || return 1
  printf '%s\n' "$dump" | awk '
    /\*UND\*/ {
      for (i = 1; i <= NF; i++) {
        token = $i
        gsub(/[()]/, "", token)
        if (token ~ /^(GLIBC|GLIBCXX|CXXABI)_[0-9]+(\.[0-9]+)*$/) print token
      }
    }
  ' | sort -u
}

scan_one() {
  bin=$1
  if [ ! -f "$bin" ]; then
    echo "no such binary: $bin" >&2
    return 1
  fi
  max_glibc=none
  max_glibcxx=none
  max_cxxabi=none
  violated=0
  versions=$(needed_versions "$bin") || {
    echo "objdump failed on $bin" >&2
    return 1
  }
  for sym in $versions; do
    case "$sym" in
      GLIBC_*)
        ver=${sym#GLIBC_}
        floor=$GLIBC_FLOOR
        if [ "$max_glibc" = none ] || [ "$(newer "$ver" "$max_glibc")" = yes ]; then
          max_glibc=$ver
        fi
        ;;
      GLIBCXX_*)
        ver=${sym#GLIBCXX_}
        floor=$GLIBCXX_FLOOR
        if [ "$max_glibcxx" = none ] || [ "$(newer "$ver" "$max_glibcxx")" = yes ]; then
          max_glibcxx=$ver
        fi
        ;;
      CXXABI_*)
        ver=${sym#CXXABI_}
        floor=$CXXABI_FLOOR
        if [ "$max_cxxabi" = none ] || [ "$(newer "$ver" "$max_cxxabi")" = yes ]; then
          max_cxxabi=$ver
        fi
        ;;
      *)
        continue
        ;;
    esac
    if [ "$(newer "$ver" "$floor")" = yes ]; then
      echo "$bin needs $sym, above the floor $floor" >&2
      violated=1
    fi
  done
  echo "$bin: GLIBC $max_glibc, GLIBCXX $max_glibcxx, CXXABI $max_cxxabi"
  return "$violated"
}

if [ "${1:-}" = --self-test ]; then
  self_test
  exit 0
fi

if [ "$#" -eq 0 ]; then
  echo "usage: $0 --self-test | binary..." >&2
  exit 1
fi

fail=0
for bin in "$@"; do
  scan_one "$bin" || fail=1
done
exit "$fail"
