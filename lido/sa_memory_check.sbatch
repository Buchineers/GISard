#!/bin/bash
#SBATCH --job-name=gisard-sa-memory
#SBATCH --account=fak04
#SBATCH --partition=long
#SBATCH --nodes=1
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=20
#SBATCH --exclusive
#SBATCH --time=04:00:00
#SBATCH --mem=60G
#SBATCH --output=logs/sa-memory-%j.out
#SBATCH --error=logs/sa-memory-%j.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
: "${GISARD_TIER:?missing GISARD_TIER}"
: "${GISARD_SPACING:?missing GISARD_SPACING}"
: "${GISARD_CHAINS:?missing GISARD_CHAINS}"
: "${GISARD_WORKERS:?missing GISARD_WORKERS}"
: "${GISARD_MATRIX_LIMIT_MB:?missing GISARD_MATRIX_LIMIT_MB}"
: "${GISARD_PROBE_JOB_ID:?missing GISARD_PROBE_JOB_ID}"

export RAYON_NUM_THREADS=$GISARD_CHAINS
attempt=${SLURM_RESTART_COUNT:-0}
metrics_dir=$GISARD_CAMPAIGN/memory-checks/metrics-${GISARD_TIER}-${GISARD_PROBE_JOB_ID}-${SLURM_JOB_ID}-${attempt}
mkdir -p "$metrics_dir"

srun "$GISARD_BIN" sa-memory-check "$GISARD_CAMPAIGN" \
  --spacing "$GISARD_SPACING" \
  --chains "$GISARD_CHAINS" \
  --seconds "${GISARD_MEMORY_CHECK_SECONDS:-60}" \
  --metrics-dir "$metrics_dir"

mapfile -t metrics < <(find "$metrics_dir" -maxdepth 1 -type f -name 'task-*.tsv' -print | sort)
if ((${#metrics[@]} != GISARD_WORKERS)); then
  echo "memory guard failed: expected $GISARD_WORKERS task metrics, found ${#metrics[@]}" >&2
  exit 1
fi
read -r aggregate_guarded_mb aggregate_peak_mb < <(
  awk '{ guarded += $1; peak += $2 } END { printf "%.3f %.3f\n", guarded, peak }' "${metrics[@]}"
)
aggregate_limit_mb=$(awk -v workers="$GISARD_WORKERS" -v limit="$GISARD_MATRIX_LIMIT_MB" \
  'BEGIN { printf "%.3f", workers * limit }')
awk -v used="$aggregate_guarded_mb" -v limit="$aggregate_limit_mb" \
  'BEGIN { if (used > limit + 1.0) exit 1 }' || {
    echo "memory guard failed: guarded matrices ${aggregate_guarded_mb} MB exceed ${aggregate_limit_mb} MB" >&2
    exit 1
  }
awk -v used="$aggregate_peak_mb" 'BEGIN { if (used > 61440.0) exit 1 }' || {
  echo "memory guard failed: summed process peaks ${aggregate_peak_mb} MB exceed 60 GiB" >&2
  exit 1
}

marker=$GISARD_CAMPAIGN/memory-checks/${GISARD_TIER}-${GISARD_PROBE_JOB_ID}.ok
temporary=${marker}.${SLURM_JOB_ID}.tmp
printf 'job=%s tier=%s spacing=%s tasks=%s chains=%s guarded_mb=%s guarded_limit_mb=%s summed_peak_mb=%s completed=%s\n' \
  "$SLURM_JOB_ID" "$GISARD_TIER" "$GISARD_SPACING" "${SLURM_NTASKS:-unknown}" \
  "$GISARD_CHAINS" "$aggregate_guarded_mb" "$aggregate_limit_mb" "$aggregate_peak_mb" \
  "$(date --iso-8601=seconds)" >"$temporary"
mv "$temporary" "$marker"
echo "memory guard passed: $marker"
