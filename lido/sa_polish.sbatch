#!/bin/bash
#SBATCH --job-name=gisard-polish
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=20
#SBATCH --time=01:00:00
#SBATCH --mem=32G
#SBATCH --output=logs/polish-%j.out
#SBATCH --error=logs/polish-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-1}

"$GISARD_BIN" status "$GISARD_CAMPAIGN"
srun "$GISARD_BIN" polish "$GISARD_CAMPAIGN" \
  --radius 60 \
  --windows 100 \
  --rounds 1
