#!/bin/bash
#SBATCH --job-name=gisard-sa-submit
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=1
#SBATCH --time=00:10:00
#SBATCH --mem=1G
#SBATCH --output=logs/sa-submit-%j.out
#SBATCH --error=logs/sa-submit-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_SCRIPT_DIR=${GISARD_SCRIPT_DIR:-${GISARD_ROOT}/lido}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
: "${GISARD_PROBE_JOB_ID:?missing GISARD_PROBE_JOB_ID}"

while IFS=$'\t' read -r tier rest; do
  [[ $tier != tier ]] || continue
  marker=$GISARD_CAMPAIGN/memory-checks/${tier}-${GISARD_PROBE_JOB_ID}.ok
  if [[ ! -s $marker ]]; then
    echo "MEMORY GUARD FAILED: $tier did not publish $marker" >&2
    echo "production was not submitted; inspect logs/sa-memory-${tier}-*.err and sacct" >&2
    exit 1
  fi
  echo "verified $marker"
done <"$GISARD_CAMPAIGN/sa-portfolio.tsv"

"${GISARD_SCRIPT_DIR}/submit_sa_production.sh"
