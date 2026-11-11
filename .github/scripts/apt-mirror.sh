#!/usr/bin/env bash
# If the Ubuntu archive cannot be reached, switch apt sources to the first
# of these mirrors that answers, then run apt-get update with retries.
set -eu

MIRRORS=(
  http://mirrors.edge.kernel.org/ubuntu
  http://mirror.math.princeton.edu/pub/ubuntu
  http://ftp.osuosl.org/pub/ubuntu
)

APT_FAST=(
  -o Acquire::Retries=0
  -o Acquire::ForceIPv4=true
  -o Acquire::http::Timeout=8
  -o Acquire::https::Timeout=8
)

ports_of() {
  case "$1" in
    */) printf '%sports\n' "$1" ;;
    *) printf '%s-ports\n' "$1" ;;
  esac
}

answers() {
  curl -fsI -o /dev/null -L --connect-timeout 5 --max-time 8 "$1/" 2>/dev/null
}

rewrite_sources() {
  local archive="$1"
  local ports
  ports="$(ports_of "$archive")"
  local files=()
  local f
  for f in /etc/apt/apt-mirrors.txt /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
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

apt_update() {
  timeout --foreground -k 5 20 apt-get "${APT_FAST[@]}" update
}

update_with_retries() {
  local n=1
  while [ "$n" -le 3 ]; do
    if apt_update; then
      return 0
    fi
    sleep $((n * 2))
    n=$((n + 1))
  done
  return 1
}

if answers http://archive.ubuntu.com/ubuntu || answers http://azure.archive.ubuntu.com/ubuntu; then
  if apt_update; then
    exit 0
  fi
fi

echo "the runner's Ubuntu archive did not answer; trying a public mirror" >&2

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
