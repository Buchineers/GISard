#!/bin/bash

set -euo pipefail

die() {
  echo "post-probe launch: $*" >&2
  exit 1
}

submit_job() {
  if [[ -n $RESERVATION_NAME ]]; then
    env \
      -u SLURM_NTASKS \
      -u SLURM_NPROCS \
      -u SLURM_CPUS_PER_TASK \
      -u SLURM_NNODES \
      -u SLURM_NODES \
      -u SLURM_JOB_NUM_NODES \
      -u SLURM_MEM_PER_NODE \
      -u SLURM_MEM_PER_CPU \
      -u SLURM_MEM_PER_GPU \
      -u SLURM_TIMELIMIT \
      sbatch --reservation "$RESERVATION_NAME" "$@"
  else
    env \
      -u SLURM_NTASKS \
      -u SLURM_NPROCS \
      -u SLURM_CPUS_PER_TASK \
      -u SLURM_NNODES \
      -u SLURM_NODES \
      -u SLURM_JOB_NUM_NODES \
      -u SLURM_MEM_PER_NODE \
      -u SLURM_MEM_PER_CPU \
      -u SLURM_MEM_PER_GPU \
      -u SLURM_TIMELIMIT \
      sbatch "$@"
  fi
}

write_launch_state() {
  local path=$GISARD_CAMPAIGN/launch.env
  local temporary=${path}.$$.tmp
  {
    printf 'PROBE_JOB_ID=%q\n' "$PROBE_JOB_ID"
    printf 'MEMORY_JOB_IDS=%q\n' "$MEMORY_JOB_IDS"
    printf 'SUBMIT_JOB_ID=%q\n' "$SUBMIT_JOB_ID"
    printf 'CAMPAIGN_JOB_IDS=%q\n' "$CAMPAIGN_JOB_IDS"
    printf 'CAMPAIGN_JOB_ID=%q\n' "$CAMPAIGN_JOB_IDS"
    printf 'POLISH_JOB_ID=%q\n' "$POLISH_JOB_ID"
    printf 'HARVEST_JOB_ID=%q\n' "$HARVEST_JOB_ID"
    printf 'SMOKE_MODE=%q\n' "$SMOKE_MODE"
    printf 'RESERVATION_NAME=%q\n' "$RESERVATION_NAME"
    printf 'CAMPAIGN_DIR=%q\n' "$GISARD_CAMPAIGN"
    printf 'SUBMISSION_FILE=%q\n' "$OUTPUT_FILE"
    printf 'SOURCE_REVISION=%q\n' "$SOURCE_REVISION"
    printf 'SOURCE_DIRTY=%q\n' "$SOURCE_DIRTY"
    printf 'BINARY_SHA256=%q\n' "$BINARY_SHA256"
    printf 'DATASET_SHA256=%q\n' "$DATASET_SHA256"
    printf 'SA_PAIR_PROBABILITY=%q\n' "$SA_PAIR_PROBABILITY"
    printf 'SA_TARGET_PROBABILITY=%q\n' "$SA_TARGET_PROBABILITY"
  } >"$temporary"
  mv "$temporary" "$path"
}

append_csv() {
  local current=$1
  local value=$2
  if [[ -n $current ]]; then
    printf '%s,%s\n' "$current" "$value"
  else
    printf '%s\n' "$value"
  fi
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GISARD_ROOT=${GISARD_ROOT:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
PLAN_FILE=$GISARD_CAMPAIGN/sa-portfolio.tsv
LAUNCH_FILE=$GISARD_CAMPAIGN/launch.env

[[ -s $PLAN_FILE ]] || die "probe did not write $PLAN_FILE"
[[ -f $SCRIPT_DIR/sa_memory_check.sbatch ]] || die "missing sa_memory_check.sbatch"
[[ -f $SCRIPT_DIR/sa_submit_after_checks.sbatch ]] || die "missing sa_submit_after_checks.sbatch"

PROBE_JOB_ID=${SLURM_JOB_ID:-}
MEMORY_JOB_IDS=
SUBMIT_JOB_ID=
CAMPAIGN_JOB_IDS=
POLISH_JOB_ID=
HARVEST_JOB_ID=
SMOKE_MODE=${GISARD_SMOKE:-0}
RESERVATION_NAME=${GISARD_RESERVATION:-}
OUTPUT_FILE=${GISARD_CAMPAIGN}/submission/solution.txt
SOURCE_REVISION=unknown
SOURCE_DIRTY=unknown
BINARY_SHA256=unknown
DATASET_SHA256=unknown
SA_PAIR_PROBABILITY=${GISARD_SA_PAIR_PROBABILITY:-0}
SA_TARGET_PROBABILITY=${GISARD_SA_TARGET_PROBABILITY:-0.85}
if [[ -f $LAUNCH_FILE ]]; then
  source "$LAUNCH_FILE"
fi
PROBE_JOB_ID=${PROBE_JOB_ID:-${SLURM_JOB_ID:-}}
[[ -n $PROBE_JOB_ID ]] || die "cannot identify the probe job"

mkdir -p "$GISARD_CAMPAIGN/memory-checks" "$GISARD_ROOT/logs"
DEPENDENCY=
while IFS=$'\t' read -r tier nominal nodes workers chains spacing matrix_limit measured candidates build_seconds; do
  [[ $tier != tier ]] || continue
  [[ $tier =~ ^[a-zA-Z0-9_-]+$ ]] || die "invalid tier id: $tier"
  [[ $workers =~ ^[1-9][0-9]*$ && $chains =~ ^[1-9][0-9]*$ ]] || die "invalid topology for $tier"
  ((workers * chains <= 20)) || die "$tier exceeds 20 cores"
  CHECK_EXPORT="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_SCRIPT_DIR=${SCRIPT_DIR},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${GISARD_CAMPAIGN},GISARD_RESERVATION=${RESERVATION_NAME},GISARD_TIER=${tier},GISARD_SPACING=${spacing},GISARD_CHAINS=${chains},GISARD_WORKERS=${workers},GISARD_MATRIX_LIMIT_MB=${matrix_limit},GISARD_PROBE_JOB_ID=${PROBE_JOB_ID},GISARD_MEMORY_CHECK_SECONDS=${GISARD_PROBE_SECONDS:-60}"
  submission=$(submit_job --parsable \
    --job-name "gisard-mem-${tier}" \
    --dependency "afterok:${PROBE_JOB_ID}" \
    --nodes 1 \
    --ntasks-per-node "$workers" \
    --cpus-per-task "$chains" \
    --output "$GISARD_ROOT/logs/sa-memory-${tier}-%j.out" \
    --error "$GISARD_ROOT/logs/sa-memory-${tier}-%j.err" \
    --export "$CHECK_EXPORT" \
    "$SCRIPT_DIR/sa_memory_check.sbatch")
  job_id=${submission%%;*}
  MEMORY_JOB_IDS=$(append_csv "$MEMORY_JOB_IDS" "$job_id")
  if [[ -n $DEPENDENCY ]]; then
    DEPENDENCY=${DEPENDENCY}:${job_id}
  else
    DEPENDENCY=$job_id
  fi
  echo "memory guard $tier: $job_id ($workers workers x $chains chains, spacing $spacing)"
done <"$PLAN_FILE"

[[ -n $MEMORY_JOB_IDS ]] || die "portfolio contains no tiers"
GATE_EXPORT="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_SCRIPT_DIR=${SCRIPT_DIR},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${GISARD_CAMPAIGN},GISARD_RESERVATION=${RESERVATION_NAME},GISARD_PROBE_JOB_ID=${PROBE_JOB_ID},GISARD_SA_PAIR_PROBABILITY=${SA_PAIR_PROBABILITY},GISARD_SA_TARGET_PROBABILITY=${SA_TARGET_PROBABILITY}"
submission=$(submit_job --parsable \
  --dependency "afterany:${DEPENDENCY}" \
  --output "$GISARD_ROOT/logs/sa-submit-%j.out" \
  --error "$GISARD_ROOT/logs/sa-submit-%j.err" \
  --export "$GATE_EXPORT" \
  "$SCRIPT_DIR/sa_submit_after_checks.sbatch")
SUBMIT_JOB_ID=${submission%%;*}
write_launch_state

echo "production gate: $SUBMIT_JOB_ID (runs after every memory check)"
echo "updated $LAUNCH_FILE"
