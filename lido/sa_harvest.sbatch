#!/bin/bash
#SBATCH --job-name=gisard-harvest
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=20
#SBATCH --time=00:45:00
#SBATCH --mem=16G
#SBATCH --output=logs/harvest-%j.out
#SBATCH --error=logs/harvest-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
GISARD_OUTPUT=${GISARD_OUTPUT:-${GISARD_CAMPAIGN}/submission/solution.txt}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-1}

"$GISARD_BIN" status "$GISARD_CAMPAIGN"
srun "$GISARD_BIN" harvest "$GISARD_CAMPAIGN" --out "$GISARD_OUTPUT"

echo "submission ready: $GISARD_OUTPUT"
