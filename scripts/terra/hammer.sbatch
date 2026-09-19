#!/usr/bin/env bash
#SBATCH --job-name=packset-hammer
#SBATCH --time=00:40:00
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=8G
#SBATCH --partition=cpu
#SBATCH --output=%x-%j.out
# Many clients on one writer: the hammer example at 1, 4, 16 and 32 clients,
# 200 operations each, against a scratch writer with the dense ballot on.
set -uo pipefail
export PATH="$HOME/stackbin:$PATH"
export PACKSET_PORT=8798 PACKSET_URL=http://127.0.0.1:8798 PACKSET_HOME="$HOME/.cache/packset-hammer"
export PACKSET_EMBED_MODEL=bge-small PACKSET_EMBED_CACHE="$HOME/.cache/packset-embed"
rm -rf "$PACKSET_HOME"
packset ensure
cd "$HOME/Git/Github/packset" || exit 1
cargo build --release -p packset-daemon --example hammer 2>&1 | tail -1
for c in 1 4 16 32; do
  echo "=== $c clients x 200 ops"
  ./target/release/examples/hammer "$c" 200
done
# The herd case: every client into one workspace, the count checked at the end.
export HAMMER_SHARED=1
for c in 4 16 32; do
  echo "=== $c clients x 200 ops"
  ./target/release/examples/hammer "$c" 200
done
unset HAMMER_SHARED
packset stop >/dev/null 2>&1 || true
echo HAMMER-DONE
