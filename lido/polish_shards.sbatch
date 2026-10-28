#!/bin/bash
#SBATCH --job-name=gisard-polish-shard
#SBATCH --account=fak04
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=20
#SBATCH --exclusive
#SBATCH --mem=0
#SBATCH --output=logs/polish-shard-%A_%a.out
#SBATCH --error=logs/polish-shard-%A_%a.err

set -euo pipefail

GISARD_ROOT=${GISARD_ROOT:-${SLURM_SUBMIT_DIR:?submit from the GISard root or export GISARD_ROOT}}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:?missing GISARD_CAMPAIGN}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
GISARD_SHARDS=${GISARD_SHARDS:-${GISARD_CAMPAIGN}/polish-shards}
GISARD_PLAN=${GISARD_PLAN:-${GISARD_SHARDS}/shards.plan}

GISARD_RADIUS=${GISARD_RADIUS:-60}
GISARD_ANTENNAS=${GISARD_ANTENNAS:-2}
GISARD_TIME_LIMIT=${GISARD_TIME_LIMIT:-60}
GISARD_SPACING=${GISARD_SPACING:-8}
GISARD_CENTRES=${GISARD_CENTRES:-both}
GISARD_WINDOWS=${GISARD_WINDOWS:-}

export RAYON_NUM_THREADS=${SLURM_CPUS_PER_TASK:-20}

line=$(sed -n "$((SLURM_ARRAY_TASK_ID + 1))p" "$GISARD_PLAN")
[[ -n $line ]] || { echo "no plan line for task $SLURM_ARRAY_TASK_ID" >&2; exit 1; }
read -r scenario shard of <<<"$line"

mkdir -p "$GISARD_SHARDS"
out="${GISARD_SHARDS}/${scenario}-$(printf '%04d' "$shard").json"

windows_flag=()
if [[ -n $GISARD_WINDOWS ]]; then
  windows_flag=(--windows "$GISARD_WINDOWS")
fi

echo "task $SLURM_ARRAY_TASK_ID: $scenario shard $shard/$of -> $out"
srun "$GISARD_BIN" certify-shard "$GISARD_CAMPAIGN" \
  --scenario "$scenario" \
  --shard "$shard" \
  --of "$of" \
  --out "$out" \
  --radius "$GISARD_RADIUS" \
  --antennas "$GISARD_ANTENNAS" \
  --time-limit "$GISARD_TIME_LIMIT" \
  --spacing "$GISARD_SPACING" \
  --centres "$GISARD_CENTRES" \
  ${windows_flag[@]+"${windows_flag[@]}"}
