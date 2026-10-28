#!/bin/bash

set -euo pipefail

die() {
  echo "production launch: $*" >&2
  exit 1
}

json_u64() {
  local key=$1
  local path=$2
  awk -F: -v key="\"${key}\"" '$1 ~ key {gsub(/[ ,]/, "", $2); print $2; exit}' "$path"
}

slurm_time() {
  local total=$1
  printf '%02d:%02d:%02d' "$((total / 3600))" "$(((total % 3600) / 60))" "$((total % 60))"
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

append_csv() {
  local current=$1
  local value=$2
  if [[ -n $current ]]; then
    printf '%s,%s\n' "$current" "$value"
  else
    printf '%s\n' "$value"
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

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GISARD_ROOT=${GISARD_ROOT:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
GISARD_CAMPAIGN=${GISARD_CAMPAIGN:-${GISARD_ROOT}/campaign-sa}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
PLAN_FILE=$GISARD_CAMPAIGN/sa-portfolio.tsv
MANIFEST_FILE=$GISARD_CAMPAIGN/manifest.json
LAUNCH_FILE=$GISARD_CAMPAIGN/launch.env
SLURM_DRAIN_SECONDS=600
MAX_CAMPAIGN_SECONDS=$((23 * 3600 + 30 * 60))

[[ -s $PLAN_FILE ]] || die "missing $PLAN_FILE"
[[ -f $MANIFEST_FILE ]] || die "missing $MANIFEST_FILE"
[[ -f $SCRIPT_DIR/sa_campaign.sbatch ]] || die "missing sa_campaign.sbatch"
[[ -f $SCRIPT_DIR/sa_polish.sbatch ]] || die "missing sa_polish.sbatch"
[[ -f $SCRIPT_DIR/sa_harvest.sbatch ]] || die "missing sa_harvest.sbatch"

DEADLINE_UNIX=$(json_u64 deadline_unix "$MANIFEST_FILE")
START_UNIX=$(json_u64 start_unix "$MANIFEST_FILE")
DURATION_SECONDS=$(json_u64 duration_seconds "$MANIFEST_FILE")
RESERVE_SECONDS=$(json_u64 reserve_seconds "$MANIFEST_FILE")
[[ $DEADLINE_UNIX =~ ^[0-9]+$ && $START_UNIX =~ ^[0-9]+$ && $DURATION_SECONDS =~ ^[0-9]+$ && $RESERVE_SECONDS =~ ^[0-9]+$ ]] || die "invalid campaign clock"

if ((DEADLINE_UNIX > 0)); then
  USABLE_SECONDS=$((DEADLINE_UNIX - RESERVE_SECONDS - $(date +%s)))
else
  ((DURATION_SECONDS > 0)) || die "campaign has no deadline"
  USABLE_SECONDS=$((DURATION_SECONDS - RESERVE_SECONDS))
fi
((USABLE_SECONDS >= 17 * 60)) || die "preflight finished too late to leave one 15-minute seed before reserve"

CAMPAIGN_LIMIT_SECONDS=$((USABLE_SECONDS + SLURM_DRAIN_SECONDS))
if ((CAMPAIGN_LIMIT_SECONDS > MAX_CAMPAIGN_SECONDS)); then
  CAMPAIGN_LIMIT_SECONDS=$MAX_CAMPAIGN_SECONDS
fi
CAMPAIGN_LIMIT=$(slurm_time "$CAMPAIGN_LIMIT_SECONDS")

PROBE_JOB_ID=${GISARD_PROBE_JOB_ID:-}
MEMORY_JOB_IDS=
SUBMIT_JOB_ID=${SLURM_JOB_ID:-}
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
PROBE_JOB_ID=${PROBE_JOB_ID:-${GISARD_PROBE_JOB_ID:-}}
SUBMIT_JOB_ID=${SUBMIT_JOB_ID:-${SLURM_JOB_ID:-}}

for value in "$GISARD_ROOT" "$GISARD_BIN" "$GISARD_CAMPAIGN"; do
  [[ $value != *,* ]] || die "paths containing commas cannot pass through Slurm --export: $value"
done

LAUNCH_WAVES=${GISARD_LAUNCH_WAVES:-3}
LAUNCH_WAVE_SECONDS=${GISARD_LAUNCH_WAVE_SECONDS:-60}
if [[ ${SMOKE_MODE:-0} == 1 && -z ${GISARD_LAUNCH_WAVES:-} ]]; then
  LAUNCH_WAVES=1
fi
[[ $LAUNCH_WAVES =~ ^[1-9][0-9]*$ ]] || die "invalid GISARD_LAUNCH_WAVES: $LAUNCH_WAVES"
[[ $LAUNCH_WAVE_SECONDS =~ ^[0-9]+$ ]] || die "invalid GISARD_LAUNCH_WAVE_SECONDS: $LAUNCH_WAVE_SECONDS"

DEPENDENCY=
TOTAL_NODES=0
TOTAL_CHAINS=0
while IFS=$'\t' read -r tier nominal nodes workers chains spacing matrix_limit measured candidates build_seconds; do
  [[ $tier != tier ]] || continue
  [[ $nodes =~ ^[1-9][0-9]*$ && $workers =~ ^[1-9][0-9]*$ && $chains =~ ^[1-9][0-9]*$ ]] || die "invalid portfolio row for $tier"
  ((workers * chains <= 20)) || die "$tier exceeds 20 cores"
  end=$((nodes - 1))
  COHORT_SIZE=64
  EXPORTS="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${GISARD_CAMPAIGN},GISARD_TIER=${tier},GISARD_CHAINS=${chains},GISARD_SA_COHORT=${COHORT_SIZE},GISARD_SA_PAIR_PROBABILITY=${SA_PAIR_PROBABILITY},GISARD_SA_TARGET_PROBABILITY=${SA_TARGET_PROBABILITY}"
  if [[ ${SMOKE_MODE:-0} == 1 ]]; then
    EXPORTS="${EXPORTS},GISARD_SA_BUDGETS_COLON=60:300:1200,GISARD_SA_COHORT=8"
  fi
  wave_size=$(((nodes + LAUNCH_WAVES - 1) / LAUNCH_WAVES))
  wave_start=0
  wave=0
  wave_ids=
  while ((wave_start <= end)); do
    wave_end=$((wave_start + wave_size - 1))
    ((wave_end <= end)) || wave_end=$end
    if ((wave > 0)); then
      begin="now+$((wave * LAUNCH_WAVE_SECONDS))"
    else
      begin=now
    fi
    submission=$(submit_job --parsable \
      --job-name "gisard-sa-${tier}" \
      --array "${wave_start}-${wave_end}" \
      --begin "$begin" \
      --ntasks-per-node "$workers" \
      --cpus-per-task "$chains" \
      --time "$CAMPAIGN_LIMIT" \
      --output "$GISARD_ROOT/logs/sa-${tier}-%A_%a.out" \
      --error "$GISARD_ROOT/logs/sa-${tier}-%A_%a.err" \
      --export "$EXPORTS" \
      "$SCRIPT_DIR/sa_campaign.sbatch")
    job_id=${submission%%;*}
    CAMPAIGN_JOB_IDS=$(append_csv "$CAMPAIGN_JOB_IDS" "$job_id")
    wave_ids=$(append_csv "$wave_ids" "$job_id")
    if [[ -n $DEPENDENCY ]]; then
      DEPENDENCY=${DEPENDENCY}:${job_id}
    else
      DEPENDENCY=$job_id
    fi
    echo "campaign $tier wave $((wave + 1)): $job_id (tasks ${wave_start}-${wave_end}, starts +$((wave * LAUNCH_WAVE_SECONDS))s)"
    wave_start=$((wave_end + 1))
    wave=$((wave + 1))
  done
  TOTAL_NODES=$((TOTAL_NODES + nodes))
  TOTAL_CHAINS=$((TOTAL_CHAINS + nodes * workers * chains))
  echo "campaign $tier: $wave_ids (${nodes} nodes x ${workers} workers x ${chains} chains in $wave waves, independent spacing lanes)"
done <"$PLAN_FILE"

[[ -n $CAMPAIGN_JOB_IDS ]] || die "portfolio contains no production tiers"
if [[ -z $POLISH_JOB_ID ]]; then
  POLISH_EXPORT="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${GISARD_CAMPAIGN}"
  submission=$(submit_job --parsable \
    --dependency "afterany:${DEPENDENCY}" \
    --export "$POLISH_EXPORT" \
    "$SCRIPT_DIR/sa_polish.sbatch")
  POLISH_JOB_ID=${submission%%;*}
  echo "MIP polish job: $POLISH_JOB_ID (after all production arrays)"
fi

if [[ -z $HARVEST_JOB_ID ]]; then
  HARVEST_EXPORT="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${GISARD_CAMPAIGN},GISARD_OUTPUT=${OUTPUT_FILE}"
  submission=$(submit_job --parsable \
    --dependency "afterany:${POLISH_JOB_ID}" \
    --export "$HARVEST_EXPORT" \
    "$SCRIPT_DIR/sa_harvest.sbatch")
  HARVEST_JOB_ID=${submission%%;*}
  echo "harvest job: $HARVEST_JOB_ID (after MIP polish)"
fi

write_launch_state
echo "submitted $TOTAL_NODES nodes and $TOTAL_CHAINS chains; limit $CAMPAIGN_LIMIT"
echo "updated $LAUNCH_FILE"
