#!/usr/bin/env bash
# If the Ubuntu archive cannot be reached, switch apt sources to the first
# of these mirrors that answers, then run apt-get update with retries.
set -eu

MIRRORS=(
  http://mirrors.edge.kernel.org/ubuntu
  http://mirror.math.princeton.edu/pub/ubuntu
  http://ftp.osuosl.org/pub/ubuntu
)

ports_of() {
  case "$1" in
    */) printf '%sports\n' "$1" ;;
    *) printf '%s-ports\n' "$1" ;;
  esac
}

answers() {
  curl -fsI -o /dev/null -L --connect-timeout 5 --max-time 10 "$1/" 2>/dev/null
}

rewrite_sources() {
  local archive="$1"
  local ports
  ports="$(ports_of "$archive")"
  local files=()
  local f
  for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
    if [ -f "$f" ]; then
      files+=("$f")
    fi
  done
  if [ "${#files[@]}" -eq 0 ]; then
    return 0
  fi
  sed -i -E \
    -e "s#https?://[^[:space:]]*(archive|security)\\.ubuntu\\.com/ubuntu/?#${archive}#g" \
    -e "s#https?://[^[:space:]]*ports\\.ubuntu\\.com/ubuntu-ports/?#${ports}#g" \
    "${files[@]}"
}

update_with_retries() {
  local n=1
  while [ "$n" -le 5 ]; do
    if apt-get update; then
      return 0
    fi
    sleep $((n * 2))
    n=$((n + 1))
  done
  return 1
}

if timeout 20 apt-get update; then
  exit 0
fi

echo "apt-get update against the runner's Ubuntu archive failed; trying a public mirror" >&2

mirror=""
for candidate in "${MIRRORS[@]}"; do
  if answers "$candidate"; then
    mirror="$candidate"
    break
  fi
done

if [ -z "$mirror" ]; then
  echo "no Ubuntu apt mirror answered" >&2
  exit 1
fi

echo "using $mirror" >&2
rewrite_sources "$mirror"
update_with_retries
