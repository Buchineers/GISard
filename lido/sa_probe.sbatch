#!/bin/bash
#SBATCH --job-name=gisard-sa-probe
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=20
#SBATCH --time=04:00:00
#SBATCH --mem=40G
#SBATCH --output=logs/sa-probe-%j.out
#SBATCH --error=logs/sa-probe-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_SCRIPT_DIR=${GISARD_SCRIPT_DIR:-${GISARD_ROOT}/lido}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-20}

run_probe() {
  local spacings=$1
  local portfolio=--portfolio
  if [[ ${GISARD_SMOKE:-0} == 1 ]]; then
    portfolio=--smoke-portfolio
  fi
  if [[ -n ${GISARD_NODES:-} ]]; then
    set -- --nodes "$GISARD_NODES"
  else
    set --
  fi
  srun "$GISARD_BIN" sa-probe "$GISARD_CAMPAIGN" \
    --spacings "$spacings" \
    --trial-seconds "${GISARD_PROBE_SECONDS:-60}" \
    --node-memory-mb "${GISARD_NODE_MEMORY_MB:-65536}" \
    --node-cores "${GISARD_NODE_CORES:-20}" \
    --max-workers "${GISARD_MAX_WORKERS:-5}" \
    --chains 4 \
    "$@" \
    "$portfolio"
}

spacings=${GISARD_SPACINGS_COLON:-}
if [[ -n $spacings ]]; then
  spacings=${spacings//:/,}
else
  spacings=${GISARD_SPACINGS:-64}
fi
run_probe "$spacings"

if [[ ${GISARD_AUTO_LAUNCH:-0} == 1 ]]; then
  "${GISARD_SCRIPT_DIR}/submit_sa_after_probe.sh"
fi
