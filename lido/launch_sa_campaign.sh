#!/bin/bash

set -euo pipefail

usage() {
  cat <<'EOF'
usage:
  ./lido/launch_sa_campaign.sh --dataset <geojson> --scenarios <tau:k,...> \
    (--deadline <unix-or-date> | --duration-hours <hours>)

  ./lido/launch_sa_campaign.sh --smoke --dataset <geojson> \
    --scenarios <tau:k,...>

optional:
  --campaign <path>       default: <checkout>/campaign-sa
                          smoke:   <checkout>/campaign-sa-smoke
  --smoke                 3 production nodes for 2h; 1/5/20-minute ladder
                          uses normal nodes by default
  --reservation <name>    override the Slurm reservation
  --no-reservation        submit every stage to normal nodes
  --nodes <n>             override production node count (for example: 1)
  --probe-spacings <list> default: 64 (coarse calibration start)
  --probe-seconds <n>     default: 60 of SA in each memory preflight
  --pair-probability <p>  default: 0 (the verified 12h campaign setting)
  --target-probability <p> default: 0.85

example:
  ./lido/launch_sa_campaign.sh \
    --dataset "$PWD/instances/competition.geojson" \
    --scenarios 0.75:50,0.5:250,0.25:1000 \
    --deadline '2026-08-16 18:00:00'
EOF
}

die() {
  echo "launch: $*" >&2
  exit 1
}

submit_job() {
  if [[ -n $RESERVATION_NAME ]]; then
    sbatch --reservation "$RESERVATION_NAME" "$@"
  else
    sbatch "$@"
  fi
}

absolute_path() {
  local path=$1
  if [[ $path == /* ]]; then
    printf '%s\n' "$path"
  else
    local parent
    parent=$(dirname -- "$path")
    printf '%s/%s\n' "$(cd -- "$parent" && pwd)" "$(basename -- "$path")"
  fi
}

write_launch_state() {
  local path=$CAMPAIGN/launch.env
  local temporary=${path}.$$.tmp
  {
    printf 'PROBE_JOB_ID=%q\n' "$PROBE_JOB_ID"
    printf 'MEMORY_JOB_IDS=%q\n' ""
    printf 'SUBMIT_JOB_ID=%q\n' ""
    printf 'CAMPAIGN_JOB_IDS=%q\n' ""
    printf 'CAMPAIGN_JOB_ID=%q\n' ""
    printf 'POLISH_JOB_ID=%q\n' "$POLISH_JOB_ID"
    printf 'HARVEST_JOB_ID=%q\n' "$HARVEST_JOB_ID"
    printf 'SMOKE_MODE=%q\n' "$SMOKE"
    printf 'RESERVATION_NAME=%q\n' "$RESERVATION_NAME"
    printf 'CAMPAIGN_DIR=%q\n' "$CAMPAIGN"
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
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
if [[ -n ${GISARD_CAMPAIGN+x} ]]; then
  CAMPAIGN=$GISARD_CAMPAIGN
  CAMPAIGN_EXPLICIT=1
else
  CAMPAIGN=${GISARD_ROOT}/campaign-sa
  CAMPAIGN_EXPLICIT=0
fi
DATASET=
SCENARIOS=
DEADLINE_VALUE=
DURATION_HOURS=
SMOKE=0
if [[ -n ${GISARD_RESERVATION+x} ]]; then
  RESERVATION_NAME=$GISARD_RESERVATION
  RESERVATION_MODE=explicit
else
  RESERVATION_NAME=
  RESERVATION_MODE=auto
fi
PROBE_SPACINGS=${GISARD_SPACINGS:-64}
PROBE_SECONDS=${GISARD_PROBE_SECONDS:-60}
NODE_OVERRIDE=${GISARD_NODES:-}
SA_PAIR_PROBABILITY=${GISARD_SA_PAIR_PROBABILITY:-0}
SA_TARGET_PROBABILITY=${GISARD_SA_TARGET_PROBABILITY:-0.85}
RESERVE_SECONDS=3600
MAX_DURATION_SECONDS=$((24 * 3600))

while (($#)); do
  case $1 in
    --dataset)
      (($# >= 2)) || die "--dataset needs a value"
      DATASET=$2
      shift 2
      ;;
    --scenarios)
      (($# >= 2)) || die "--scenarios needs a value"
      SCENARIOS=$2
      shift 2
      ;;
    --deadline)
      (($# >= 2)) || die "--deadline needs a value"
      DEADLINE_VALUE=$2
      shift 2
      ;;
    --duration-hours)
      (($# >= 2)) || die "--duration-hours needs a value"
      DURATION_HOURS=$2
      shift 2
      ;;
    --campaign)
      (($# >= 2)) || die "--campaign needs a value"
      CAMPAIGN=$2
      CAMPAIGN_EXPLICIT=1
      shift 2
      ;;
    --smoke)
      SMOKE=1
      shift
      ;;
    --reservation)
      (($# >= 2)) || die "--reservation needs a value"
      RESERVATION_NAME=$2
      RESERVATION_MODE=explicit
      shift 2
      ;;
    --no-reservation)
      RESERVATION_NAME=
      RESERVATION_MODE=explicit
      shift
      ;;
    --nodes)
      (($# >= 2)) || die "--nodes needs a value"
      NODE_OVERRIDE=$2
      shift 2
      ;;
    --probe-spacings)
      (($# >= 2)) || die "--probe-spacings needs a value"
      PROBE_SPACINGS=$2
      shift 2
      ;;
    --probe-seconds)
      (($# >= 2)) || die "--probe-seconds needs a value"
      PROBE_SECONDS=$2
      shift 2
      ;;
    --pair-probability)
      (($# >= 2)) || die "--pair-probability needs a value"
      SA_PAIR_PROBABILITY=$2
      shift 2
      ;;
    --target-probability)
      (($# >= 2)) || die "--target-probability needs a value"
      SA_TARGET_PROBABILITY=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n $DATASET ]] || die "--dataset is required"
[[ -n $SCENARIOS ]] || die "--scenarios is required"
if ((SMOKE)); then
  [[ -z $DEADLINE_VALUE ]] || die "--smoke uses a relative two-hour clock; do not combine it with --deadline"
  DURATION_HOURS=${DURATION_HOURS:-2}
  RESERVE_SECONDS=600
  if ((CAMPAIGN_EXPLICIT == 0)); then
    CAMPAIGN=${GISARD_ROOT}/campaign-sa-smoke
  fi
fi
if [[ $RESERVATION_MODE == auto ]]; then
  if ((SMOKE)); then
    RESERVATION_NAME=
  else
    RESERVATION_NAME=gis-cup-2026
  fi
fi
if [[ -n $DEADLINE_VALUE && -n $DURATION_HOURS ]] || [[ -z $DEADLINE_VALUE && -z $DURATION_HOURS ]]; then
  die "choose exactly one of --deadline or --duration-hours"
fi
[[ $PROBE_SECONDS =~ ^[1-9][0-9]*$ ]] || die "probe-seconds must be a positive integer"
[[ -z $NODE_OVERRIDE || $NODE_OVERRIDE =~ ^[1-9][0-9]*$ ]] || die "nodes must be a positive integer"

CAMPAIGN_PARTITION=${GISARD_PARTITION:-long}
reservation_usable_nodes() {
  local name=$1 partition=$2 nodes
  nodes=$(scontrol show reservation "$name" -o 2>/dev/null |
    grep -oE '(^| )Nodes=[^ ]+' | head -1 | cut -d= -f2)
  [[ -n $nodes && $nodes != '(null)' ]] || return 1
  sinfo -h -N -n "$nodes" -p "$partition" -o '%N|%T' 2>/dev/null |
    awk -F'|' '$2 !~ /drain|down|fail|maint|unk/ { print $1 }' |
    sort -u | wc -l
}

if [[ -z $NODE_OVERRIDE && -n $RESERVATION_NAME ]] && ((!SMOKE)); then
  if usable=$(reservation_usable_nodes "$RESERVATION_NAME" "$CAMPAIGN_PARTITION") &&
    [[ $usable =~ ^[1-9][0-9]*$ ]]; then
    NODE_OVERRIDE=$usable
    NODE_SOURCE="reservation $RESERVATION_NAME, partition $CAMPAIGN_PARTITION, healthy only"
  else
    echo "warning: cannot size $RESERVATION_NAME; falling back to the built-in portfolio" >&2
  fi
fi
[[ $PROBE_SPACINGS =~ ^[0-9]+([.][0-9]+)?(,[0-9]+([.][0-9]+)?)*$ ]] || die "invalid probe-spacings: $PROBE_SPACINGS"
[[ $SA_PAIR_PROBABILITY =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] || die "invalid pair-probability: $SA_PAIR_PROBABILITY"
[[ $SA_TARGET_PROBABILITY =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] || die "invalid target-probability: $SA_TARGET_PROBABILITY"
awk -v value="$SA_PAIR_PROBABILITY" 'BEGIN { exit !(value >= 0 && value <= 1) }' || die "pair-probability must be in [0, 1]"
awk -v value="$SA_TARGET_PROBABILITY" 'BEGIN { exit !(value >= 0 && value <= 1) }' || die "target-probability must be in [0, 1]"
[[ -z $RESERVATION_NAME || $RESERVATION_NAME =~ ^[a-zA-Z0-9_.-]+$ ]] || die "invalid reservation: $RESERVATION_NAME"
[[ -x $GISARD_BIN ]] || die "campaign binary is missing or not executable: $GISARD_BIN"
[[ -f $DATASET ]] || die "dataset does not exist: $DATASET"
for script in sa_probe.sbatch submit_sa_after_probe.sh submit_sa_production.sh \
  sa_submit_after_checks.sbatch sa_memory_check.sbatch sa_campaign.sbatch \
  sa_polish.sbatch sa_harvest.sbatch; do
  [[ -f $SCRIPT_DIR/$script ]] || die "missing $SCRIPT_DIR/$script"
done

DATASET=$(absolute_path "$DATASET")
CAMPAIGN=$(absolute_path "$CAMPAIGN")
if [[ -s $GISARD_ROOT/bin/campaign.source-revision && -s $GISARD_ROOT/bin/campaign.source-dirty ]]; then
  SOURCE_REVISION=$(<"$GISARD_ROOT/bin/campaign.source-revision")
  SOURCE_DIRTY=$(<"$GISARD_ROOT/bin/campaign.source-dirty")
else
  SOURCE_REVISION=$(git -C "$GISARD_ROOT" rev-parse HEAD 2>/dev/null || printf unknown)
  if git -C "$GISARD_ROOT" diff --quiet --ignore-submodules HEAD 2>/dev/null && \
     [[ -z $(git -C "$GISARD_ROOT" ls-files --others --exclude-standard 2>/dev/null) ]]; then
    SOURCE_DIRTY=0
  else
    SOURCE_DIRTY=1
  fi
fi
[[ $SOURCE_REVISION =~ ^[0-9a-f]{40}$ || $SOURCE_REVISION == unknown ]] || die "invalid deployed source revision"
[[ $SOURCE_DIRTY == 0 || $SOURCE_DIRTY == 1 ]] || die "invalid deployed source dirty flag"
BINARY_SHA256=$(sha256sum "$GISARD_BIN" | awk '{print $1}')
DATASET_SHA256=$(sha256sum "$DATASET" | awk '{print $1}')
for state_dir in results best solutions queue/running; do
  if [[ -d $CAMPAIGN/$state_dir ]] && [[ -n $(find "$CAMPAIGN/$state_dir" -type f -print -quit 2>/dev/null) ]]; then
    die "$CAMPAIGN already contains campaign work in $state_dir; use a fresh --campaign path"
  fi
done

NOW_UNIX=$(date +%s)
INIT_CLOCK_ARGS=()
if [[ -n $DEADLINE_VALUE ]]; then
  if [[ $DEADLINE_VALUE =~ ^[0-9]+$ ]]; then
    DEADLINE_UNIX=$DEADLINE_VALUE
  elif ! DEADLINE_UNIX=$(date -d "$DEADLINE_VALUE" +%s); then
    die "cannot parse deadline: $DEADLINE_VALUE"
  fi
  REMAINING_SECONDS=$((DEADLINE_UNIX - NOW_UNIX))
  ((REMAINING_SECONDS <= MAX_DURATION_SECONDS)) || die "deadline is more than 24 hours away; this launcher is for one LiDO campaign window"
  ((REMAINING_SECONDS >= RESERVE_SECONDS + 17 * 60)) || die "deadline leaves less than 17 minutes of search before the one-hour reserve"
  INIT_CLOCK_ARGS=(--deadline "$DEADLINE_UNIX")
  CLOCK_DESCRIPTION="fixed deadline $(date -d "@${DEADLINE_UNIX}" --iso-8601=seconds)"
else
  [[ $DURATION_HOURS =~ ^[0-9]+([.][0-9]+)?$ ]] || die "invalid duration-hours: $DURATION_HOURS"
  DURATION_SECONDS=$(awk -v hours="$DURATION_HOURS" 'BEGIN { printf "%.0f", hours * 3600 }')
  ((DURATION_SECONDS <= MAX_DURATION_SECONDS)) || die "duration-hours must not exceed the 24-hour LiDO campaign window"
  ((DURATION_SECONDS >= RESERVE_SECONDS + 17 * 60)) || die "duration leaves less than 17 minutes of search before the one-hour reserve"
  INIT_CLOCK_ARGS=(--duration-hours "$DURATION_HOURS")
  CLOCK_DESCRIPTION="${DURATION_HOURS}h after the first campaign worker starts"
fi

mkdir -p "$GISARD_ROOT/logs"
cd "$GISARD_ROOT"
"$GISARD_BIN" init "$CAMPAIGN" \
  --dataset "$DATASET" \
  --scenarios "$SCENARIOS" \
  "${INIT_CLOCK_ARGS[@]}" \
  --reserve "$RESERVE_SECONDS"

for value in "$GISARD_ROOT" "$GISARD_BIN" "$CAMPAIGN" "$SCRIPT_DIR"; do
  [[ $value != *,* ]] || die "paths containing commas cannot be passed through Slurm --export: $value"
done
PROBE_SPACINGS_EXPORT=${PROBE_SPACINGS//,/:}
SBATCH_EXPORT="ALL,GISARD_ROOT=${GISARD_ROOT},GISARD_SCRIPT_DIR=${SCRIPT_DIR},GISARD_BIN=${GISARD_BIN},GISARD_CAMPAIGN=${CAMPAIGN},GISARD_AUTO_LAUNCH=1,GISARD_SMOKE=${SMOKE},GISARD_RESERVATION=${RESERVATION_NAME},GISARD_SPACINGS_COLON=${PROBE_SPACINGS_EXPORT},GISARD_PROBE_SECONDS=${PROBE_SECONDS},GISARD_NODES=${NODE_OVERRIDE},GISARD_SA_PAIR_PROBABILITY=${SA_PAIR_PROBABILITY},GISARD_SA_TARGET_PROBABILITY=${SA_TARGET_PROBABILITY}"

PROBE_SUBMISSION=$(submit_job --parsable \
  --export "$SBATCH_EXPORT" \
  "$SCRIPT_DIR/sa_probe.sbatch")
PROBE_JOB_ID=${PROBE_SUBMISSION%%;*}
MEMORY_JOB_IDS=
SUBMIT_JOB_ID=
CAMPAIGN_JOB_IDS=
CAMPAIGN_JOB_ID=
POLISH_JOB_ID=
HARVEST_JOB_ID=
OUTPUT_FILE=${CAMPAIGN}/submission/solution.txt
write_launch_state
echo "probe/launcher job: $PROBE_JOB_ID"

echo "clock:              $CLOCK_DESCRIPTION"
if [[ -n $RESERVATION_NAME ]]; then
  echo "reservation:        $RESERVATION_NAME"
else
  echo "reservation:        none (normal nodes)"
fi
if [[ -n $NODE_OVERRIDE ]]; then
  echo "portfolio:          ${NODE_OVERRIDE} nodes x 5 workers x 4 chains (${NODE_SOURCE:-explicit override})"
elif ((SMOKE)); then
  echo "portfolio:          smoke: 3 nodes x 5 workers x 4 chains"
  echo "SA ladder:          1m, 5m, 20m; workers stop after 1h50m, then 10m drain"
else
  echo "portfolio:          200 nodes x 5 workers x 4 chains"
fi
echo "matrix guard:       8 GiB per worker (40 GiB across five matrices)"
echo "probe:              starts at spacing ${PROBE_SPACINGS}, discovers finest safe spacing"
echo "density:            production uses three independent coarse spacing lanes"
echo "SA moves:           pair ${SA_PAIR_PROBABILITY}, target ${SA_TARGET_PROBABILITY}"
echo "provenance:         ${SOURCE_REVISION} dirty=${SOURCE_DIRTY}, binary ${BINARY_SHA256}"
echo "dataset SHA-256:    $DATASET_SHA256"
echo "polish:             after SA completes, exact window MIP then harvest"
echo "final reserve:      $((RESERVE_SECONDS / 60)) minutes"
echo "submission:         $OUTPUT_FILE"
echo "job IDs:            $CAMPAIGN/launch.env"
echo "status:             $SCRIPT_DIR/status_sa_campaign.sh $CAMPAIGN"
echo "memory-check IDs appear after the probe; campaign IDs appear only after all guards pass"
