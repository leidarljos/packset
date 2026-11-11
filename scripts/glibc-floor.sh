#!/usr/bin/env bash
# Fail when a binary's dynamic table names a GLIBC_ symbol newer than the floor.
#
# The floor is the glibc on Ubuntu 22.04, which is 2.35. A newer symbol means
# the archive will not start on that release.
#
#   scripts/glibc-floor.sh 2.35 path/to/packset path/to/packsetd
set -euo pipefail

floor=${1:?floor such as 2.35}
shift

floor_major=${floor%%.*}
floor_rest=${floor#*.}
floor_minor=${floor_rest%%.*}
case "$floor_major$floor_minor" in
  *[!0-9]*)
    echo "floor must be major.minor, not $floor" >&2
    exit 1
    ;;
esac

if [ "$#" -eq 0 ]; then
  echo "name at least one binary" >&2
  exit 1
fi

failed=0
for bin in "$@"; do
  too_new=$(
    objdump -T "$bin" | awk -v major="$floor_major" -v minor="$floor_minor" '
      {
        for (i = 1; i <= NF; i++) {
          if ($i ~ /GLIBC_[0-9]/) {
            v = $i
            sub(/.*GLIBC_/, "", v)
            sub(/[^0-9.].*/, "", v)
            n = split(v, a, ".")
            have_major = a[1] + 0
            have_minor = (n > 1 ? a[2] + 0 : 0)
            if (have_major > major || (have_major == major && have_minor > minor)) print v
          }
        }
      }
    ' | sort -u | paste -sd ' ' -
  )
  if [ -n "$too_new" ]; then
    printf '%s: glibc newer than %s: %s\n' "$bin" "$floor" "$too_new" >&2
    failed=1
  fi
done

exit "$failed"
