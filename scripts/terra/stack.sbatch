#!/usr/bin/env bash
#SBATCH --job-name=packset-stack
#SBATCH --time=00:45:00
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=4
#SBATCH --mem=8G
#SBATCH --partition=cpu
#SBATCH --output=%x-%j.out
# Build the pack's binaries on the build host and put them where the seat's
# smoke and scale scripts look: $HOME/stackbin.
set -uo pipefail
cd "$HOME/Git/Github/packset" || exit 1
mkdir -p "$HOME/stackbin"
cargo fmt --all --check && echo FMT-CLEAN
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^(error|warning)" -A 8 | head -40
cargo build --release --workspace 2>&1 | tail -1
# The full log stays beside the job output; a failing test's panic is read
# from there rather than rerun.
cargo test --workspace --no-fail-fast > "$HOME/packset-test-${SLURM_JOB_ID}.log" 2>&1
grep -E "^(test result:|failures:|---- )" "$HOME/packset-test-${SLURM_JOB_ID}.log" | head -20
grep -E "panicked at|assertion|left:|right:" "$HOME/packset-test-${SLURM_JOB_ID}.log" | head -20
for b in packset packsetd packset-embed packset-mcp; do
  cp -f "target/release/$b" "$HOME/stackbin/" && echo "copied $b"
done
ls -la "$HOME/stackbin" | tail -5
echo STACK-DONE
