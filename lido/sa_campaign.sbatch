#!/bin/bash
#SBATCH --job-name=gisard-sa
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --array=0-209
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=5
#SBATCH --cpus-per-task=4
#SBATCH --exclusive
#SBATCH --time=23:30:00
#SBATCH --mem=0
#SBATCH --output=logs/sa-%A_%a.out
#SBATCH --error=logs/sa-%A_%a.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}

: "${GISARD_TIER:?missing GISARD_TIER}"
: "${GISARD_CHAINS:?missing GISARD_CHAINS}"
export RAYON_NUM_THREADS=$GISARD_CHAINS

run_worker() {
  export GISARD_WORKER_PREFIX="${GISARD_TIER}-a${SLURM_ARRAY_TASK_ID}"
  srun bash -c 'exec "$@" --worker "${GISARD_WORKER_PREFIX}t${SLURM_PROCID}"' \
    _ "$GISARD_BIN" work "$GISARD_CAMPAIGN" \
    --solver sa \
    --chains "$RAYON_NUM_THREADS" \
    "$@"
}

WORKER_ARGS=()
if [[ -n ${GISARD_SA_BUDGETS_COLON:-} ]]; then
  WORKER_ARGS+=(--budgets "${GISARD_SA_BUDGETS_COLON//:/,}")
fi
if [[ -n ${GISARD_SA_COHORT:-} ]]; then
  WORKER_ARGS+=(--cohort-size "$GISARD_SA_COHORT")
fi
if [[ -n ${GISARD_SA_PAIR_PROBABILITY:-} ]]; then
  WORKER_ARGS+=(--pair-probability "$GISARD_SA_PAIR_PROBABILITY")
fi
if [[ -n ${GISARD_SA_TARGET_PROBABILITY:-} ]]; then
  WORKER_ARGS+=(--target-probability "$GISARD_SA_TARGET_PROBABILITY")
fi
run_worker ${WORKER_ARGS[@]+"${WORKER_ARGS[@]}"}
