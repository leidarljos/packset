#!/usr/bin/env bash
# Rewrite fixtures through apt-mirror.sh --dry-run. curl and dpkg are fakes,
# so the test does not touch the network, apt, or /etc/apt.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script=$here/apt-mirror.sh

shellcheck "$script" "$0"

codename=$(sed -n 's/^VERSION_CODENAME=//p' /etc/os-release | head -n 1)
codename=${codename%\"}
codename=${codename#\"}
if [ -z "$codename" ]; then
  echo "VERSION_CODENAME is empty" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/apt/sources.list.d" "$tmp/expect"

cat >"$tmp/bin/dpkg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" != "--print-architecture" ]; then
  printf 'unexpected dpkg args: %s\n' "$*" >&2
  exit 1
fi
printf '%s\n' "${FAKE_ARCH:?}"
EOF

cat >"$tmp/bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
url=""
for arg in "$@"; do
  url=$arg
done
printf '%s\n' "$url" >>"${CURL_LOG:?}"
ok=0
while IFS= read -r line || [ -n "$line" ]; do
  if [ -z "$line" ]; then
    continue
  fi
  case "$url" in
    "$line"*) ok=1 ;;
  esac
done <<<"${CURL_OK:-}"
if [ "$ok" -eq 1 ]; then
  exit 0
fi
exit 22
EOF

cat >"$tmp/bin/apt-get" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' apt-get >>"${SENTINEL:?}"
exit 99
EOF

cat >"$tmp/bin/timeout" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' timeout >>"${SENTINEL:?}"
exit 99
EOF

chmod +x "$tmp/bin/dpkg" "$tmp/bin/curl" "$tmp/bin/apt-get" "$tmp/bin/timeout"

sentinel=$tmp/called
curl_log=$tmp/curl.log
export SENTINEL=$sentinel CURL_LOG=$curl_log

write_fixtures() {
  cat >"$tmp/apt/sources.list" <<'EOF'
deb http://ports.ubuntu.com/ubuntu-ports noble main restricted
deb http://archive.ubuntu.com/ubuntu noble main restricted
deb http://security.ubuntu.com/ubuntu noble-security main restricted
deb http://example.com/debian noble main
EOF
  cat >"$tmp/apt/sources.list.d/ubuntu.sources" <<'EOF'
Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports
Suites: noble noble-updates noble-backports
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: https://ports.ubuntu.com/ubuntu-ports/
Suites: noble-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
  printf '%s\n' 'deb http://ports.ubuntu.com/ubuntu-ports noble universe' \
    >"$tmp/apt/sources.list.d/extra.list"
  printf '%s\n' 'https://ports.ubuntu.com/ubuntu-ports' 'http://archive.ubuntu.com/ubuntu/' \
    >"$tmp/apt/apt-mirrors.txt"
}

save_fixtures() {
  rm -rf "$tmp/orig"
  mkdir -p "$tmp/orig/sources.list.d"
  cp "$tmp/apt/sources.list" "$tmp/orig/sources.list"
  cp "$tmp/apt/apt-mirrors.txt" "$tmp/orig/apt-mirrors.txt"
  cp "$tmp/apt/sources.list.d/ubuntu.sources" "$tmp/orig/sources.list.d/ubuntu.sources"
  cp "$tmp/apt/sources.list.d/extra.list" "$tmp/orig/sources.list.d/extra.list"
}

assert_unchanged() {
  diff -u "$tmp/orig/sources.list" "$tmp/apt/sources.list"
  diff -u "$tmp/orig/apt-mirrors.txt" "$tmp/apt/apt-mirrors.txt"
  diff -u "$tmp/orig/sources.list.d/ubuntu.sources" "$tmp/apt/sources.list.d/ubuntu.sources"
  diff -u "$tmp/orig/sources.list.d/extra.list" "$tmp/apt/sources.list.d/extra.list"
}

assert_rewritten() {
  local mirror=$1
  local ports=$2
  cat >"$tmp/expect/sources.list" <<EOF
deb ${ports} noble main restricted
deb ${mirror} noble main restricted
deb ${mirror} noble-security main restricted
deb http://example.com/debian noble main
EOF
  cat >"$tmp/expect/ubuntu.sources" <<EOF
Types: deb
URIs: ${ports}
Suites: noble noble-updates noble-backports
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: ${ports}
Suites: noble-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
  printf '%s\n' "deb ${ports} noble universe" >"$tmp/expect/extra.list"
  printf '%s\n' "$ports" "$mirror" >"$tmp/expect/apt-mirrors.txt"
  diff -u "$tmp/expect/sources.list" "$tmp/apt/sources.list"
  diff -u "$tmp/expect/ubuntu.sources" "$tmp/apt/sources.list.d/ubuntu.sources"
  diff -u "$tmp/expect/extra.list" "$tmp/apt/sources.list.d/extra.list"
  diff -u "$tmp/expect/apt-mirrors.txt" "$tmp/apt/apt-mirrors.txt"
}

run_case() {
  local name=$1
  local arch=$2
  local ok=$3
  local want=$4
  shift 4
  printf '== %s\n' "$name"
  write_fixtures
  save_fixtures
  : >"$sentinel"
  : >"$curl_log"
  local output rc
  set +e
  output=$(
    PATH="$tmp/bin:$PATH" \
      FAKE_ARCH="$arch" \
      CURL_OK="$ok" \
      APT_MIRROR_ROOT="$tmp/apt" \
      bash "$script" --dry-run 2>&1
  )
  rc=$?
  set -e
  if [ "$rc" -ne "$want" ]; then
    printf 'exit %s, want %s\n%s\n' "$rc" "$want" "$output" >&2
    exit 1
  fi
  if [ -s "$sentinel" ]; then
    printf 'apt ran during %s\n' "$name" >&2
    cat "$sentinel" >&2
    exit 1
  fi
  case_output=$output
}

case_output=""

ports=http://ports.ubuntu.com/ubuntu-ports
berkeley=http://mirrors.ocf.berkeley.edu/ubuntu-ports
chemnitz=http://ftp.tu-chemnitz.de/pub/linux/ubuntu-ports
kernel=http://mirrors.edge.kernel.org/ubuntu

run_case "arm64 uses the second ports mirror" arm64 \
  "${berkeley}/dists/${codename}/Release" 0
printf '%s\n' "$case_output" | grep -F -x "using ${berkeley}" >/dev/null
grep -F -x "${ports}/dists/${codename}/Release" "$curl_log" >/dev/null
grep -F -x "${berkeley}/dists/${codename}/Release" "$curl_log" >/dev/null
if grep -F -q "$chemnitz" "$curl_log"; then
  echo "kept probing after a mirror answered" >&2
  exit 1
fi
if grep -F -q 'mirrors.edge.kernel.org' "$curl_log"; then
  echo "arm64 probed an amd64 mirror" >&2
  exit 1
fi
if grep -F -qx "${ports}/" "$curl_log"; then
  echo "arm64 probed a mirror root instead of Release" >&2
  exit 1
fi
assert_rewritten "$berkeley" "$berkeley"

run_case "arm64 leaves sources when nothing answers" arm64 "" 0
printf '%s\n' "$case_output" | grep -F -x "no ubuntu-ports mirror answered" >/dev/null
for mirror in "$ports" "$berkeley" "$chemnitz"; do
  grep -F -x "${mirror}/dists/${codename}/Release" "$curl_log" >/dev/null
done
if grep -F -q 'mirrors.edge.kernel.org' "$curl_log"; then
  echo "arm64 probed an amd64 mirror" >&2
  exit 1
fi
assert_unchanged

run_case "official archive skips the fallback" arm64 \
  "http://archive.ubuntu.com/ubuntu/" 0
printf '%s\n' "$case_output" | grep -F -x "official archive answered" >/dev/null
if grep -F -q '/dists/' "$curl_log"; then
  echo "probed a mirror after the official archive answered" >&2
  exit 1
fi
assert_unchanged

run_case "amd64 still derives ubuntu-ports from the archive mirror" amd64 \
  "${kernel}/" 0
printf '%s\n' "$case_output" | grep -F -x "using ${kernel}" >/dev/null
grep -F -qx "${kernel}/" "$curl_log" >/dev/null
if grep -F -q '/dists/' "$curl_log"; then
  echo "amd64 probed a Release file" >&2
  exit 1
fi
assert_rewritten "$kernel" "${kernel}-ports"

run_case "amd64 fails when no mirror answers" amd64 "" 1
printf '%s\n' "$case_output" | grep -F -x "no Ubuntu apt mirror answered" >/dev/null
assert_unchanged

printf 'ok\n'
