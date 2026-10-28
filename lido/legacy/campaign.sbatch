#!/bin/bash
#SBATCH --job-name=giscup
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --reservation=gis-cup-2026
#SBATCH --array=0-209
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=5
#SBATCH --cpus-per-task=4
#SBATCH --exclusive
#SBATCH --time=20:00:00
#SBATCH --mem=0
#SBATCH --output=logs/legacy-campaign-%A_%a.out

set -euo pipefail
GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
DIR=${GISARD_DIR:-${GISARD_ROOT}/campaign}
BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-4}

srun "$BIN" work "$DIR" --worker "a${SLURM_ARRAY_TASK_ID}t${SLURM_PROCID}"
