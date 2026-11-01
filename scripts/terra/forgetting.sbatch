#!/usr/bin/env bash
#SBATCH --job-name=packset-forgetting
#SBATCH --time=00:30:00
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=4
#SBATCH --mem=8G
#SBATCH --partition=cpu
#SBATCH --output=%x-%j.out
# The decay slot and the recall order on the synthetic longitudinal corpus:
# off, age decay, FSRS, LRU and keep-testing, with the JSON beside the table.
set -uo pipefail
cd "$HOME/Git/Github/packset" || exit 1
git pull -q --ff-only 2>&1 | tail -1; git log --oneline -1 | cut -c1-70
FORGETTING_JSON="$HOME/forgetting-$SLURM_JOB_ID.json" cargo run --release -p packset-daemon --example forgetting 2>&1 | grep -vE "Compiling|Finished|Running|warning"
echo FORGETTING-DONE
