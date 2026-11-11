#!/usr/bin/env bash
# If the Ubuntu archive cannot be reached, switch apt sources to the first
# of these mirrors that answers, then run apt-get update with retries.
# arm64 packages are on the ports archive, so that architecture probes
# ubuntu-ports mirrors for dists/<codename>/Release instead.
# --dry-run rewrites sources and does not run apt-get. APT_MIRROR_ROOT,
# when set, is the directory rewritten instead of /etc/apt.
set -eu

MIRRORS=(
  http://mirrors.edge.kernel.org/ubuntu
  http://mirror.math.princeton.edu/pub/ubuntu
  http://ftp.osuosl.org/pub/ubuntu
)

# These publish the ports tree. Deriving ubuntu-ports from an amd64 mirror
# does not: mirrors.edge.kernel.org/ubuntu-ports is a 404.
PORTS_MIRRORS=(
  http://ports.ubuntu.com/ubuntu-ports
  http://mirrors.ocf.berkeley.edu/ubuntu-ports
  http://ftp.tu-chemnitz.de/pub/linux/ubuntu-ports
)

APT_FAST=(
  -o Acquire::Retries=0
  -o Acquire::ForceIPv4=true
  -o Acquire::http::Timeout=8
  -o Acquire::https::Timeout=8
)

dry_run=0
if [ "${1:-}" = "--dry-run" ]; then
  dry_run=1
fi

apt_root=/etc/apt
if [ "$dry_run" -eq 1 ] && [ -n "${APT_MIRROR_ROOT:-}" ]; then
  apt_root=$APT_MIRROR_ROOT
fi

ports_of() {
  case "$1" in
    */) printf '%sports\n' "$1" ;;
    *) printf '%s-ports\n' "$1" ;;
  esac
}

answers() {
  curl -fsI -o /dev/null -L --connect-timeout 5 --max-time 8 "$1/" 2>/dev/null
}

release_answers() {
  curl -fsI -o /dev/null -L --connect-timeout 5 --max-time 8 "$1" 2>/dev/null
}

ubuntu_codename() {
  local line=""
  if [ ! -f /etc/os-release ]; then
    return 1
  fi
  line=$(sed -n 's/^VERSION_CODENAME=//p' /etc/os-release | head -n 1)
  if [ -z "$line" ]; then
    line=$(sed -n 's/^UBUNTU_CODENAME=//p' /etc/os-release | head -n 1)
  fi
  line=${line%\"}
  line=${line#\"}
  if [ -z "$line" ]; then
    return 1
  fi
  printf '%s\n' "$line"
}

rewrite_sources() {
  local archive="$1"
  local ports="${2:-}"
  if [ -z "$ports" ]; then
    ports="$(ports_of "$archive")"
  fi
  local files=()
  local f
  for f in "$apt_root/apt-mirrors.txt" "$apt_root/sources.list" "$apt_root/sources.list.d/"*.list "$apt_root/sources.list.d/"*.sources; do
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

finish_with() {
  local archive="$1"
  local ports="${2:-}"
  echo "using $archive" >&2
  if [ -n "$ports" ]; then
    rewrite_sources "$archive" "$ports"
  else
    rewrite_sources "$archive"
  fi
  if [ "$dry_run" -eq 1 ]; then
    exit 0
  fi
  update_with_retries
}

if answers http://archive.ubuntu.com/ubuntu || answers http://azure.archive.ubuntu.com/ubuntu; then
  if [ "$dry_run" -eq 1 ]; then
    echo "official archive answered" >&2
    exit 0
  fi
  if apt_update; then
    exit 0
  fi
fi

arch=$(dpkg --print-architecture)
if [ "$arch" = "arm64" ]; then
  echo "the runner's Ubuntu archive did not answer; trying an ubuntu-ports mirror" >&2
  codename=$(ubuntu_codename) || {
    echo "no Ubuntu codename; leaving apt sources unchanged" >&2
    exit 0
  }
  mirror=""
  for candidate in "${PORTS_MIRRORS[@]}"; do
    if release_answers "$candidate/dists/$codename/Release"; then
      mirror="$candidate"
      break
    fi
  done
  if [ -z "$mirror" ]; then
    echo "no ubuntu-ports mirror answered" >&2
    exit 0
  fi
  finish_with "$mirror" "$mirror"
else
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
  finish_with "$mirror"
fi
