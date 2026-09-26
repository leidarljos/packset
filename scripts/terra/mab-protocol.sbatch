#!/usr/bin/env bash
#SBATCH --job-name=packset-mab-protocol
#SBATCH --time=04:00:00
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=32G
#SBATCH --partition=cpu
#SBATCH --output=%x-%j.out
# Write-policy MemoryAgentBench: Remember / Prefer / Accept only, all four
# competencies. Lexical retrieval over admitted claims. The encoder is off
# so the table is the protocol, not a dense ablation.
set -euo pipefail
cd "$HOME/Git/Github/packset"
export PACKSET_MAB_LEXICAL=1
export PACKSET_MAB_DUMP="$HOME/var/scratch/packset-mab-protocol-${SLURM_JOB_ID}.jsonl"
mkdir -p "$(dirname "$PACKSET_MAB_DUMP")"
cargo test --locked -p packset-core --lib mab -- --nocapture
cargo run --release --locked -p packset-daemon --example memoryagentbench -- "$HOME/data/mab"
echo MAB-PROTOCOL-DONE
