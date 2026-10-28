#!/bin/bash
#SBATCH --job-name=gisard-polish-merge
#SBATCH --account=fak04
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=20
#SBATCH --time=00:30:00
#SBATCH --mem=32G
#SBATCH --output=logs/polish-merge-%j.out
#SBATCH --error=logs/polish-merge-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:?missing GISARD_CAMPAIGN}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
GISARD_SHARDS=${GISARD_SHARDS:-${GISARD_CAMPAIGN}/polish-shards}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-20}

echo "shard files: $(find "$GISARD_SHARDS" -name '*.json' | wc -l)"
srun "$GISARD_BIN" polish-merge "$GISARD_CAMPAIGN" --shards "$GISARD_SHARDS"
"$GISARD_BIN" status "$GISARD_CAMPAIGN" --compact || true
