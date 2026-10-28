#!/bin/bash

set -uo pipefail

usage() {
  cat >&2 <<'EOF'
usage: ./lido/status_sa_campaign.sh <campaign-dir> [options]

  --watch [seconds]  redraw continuously (default: 30 seconds)
  --events <n>       show the newest worker lifecycle events (default: 10)
  --claims           include individual running claims
  --verbose          show phase explanations and per-rung solver detail
EOF
  exit 2
}

[[ $# -ge 1 ]] || usage
CAMPAIGN=$1
shift
CLAIMS=0
VERBOSE=0
WATCH=0
WATCH_SECONDS=30
EVENTS=10
ONCE=0
while (($#)); do
  case $1 in
    --watch)
      WATCH=1
      if [[ ${2:-} =~ ^[1-9][0-9]*$ ]]; then
        WATCH_SECONDS=$2
        shift 2
      else
        shift
      fi
      ;;
    --events)
      [[ ${2:-} =~ ^[0-9]+$ ]] || usage
      EVENTS=$2
      shift 2
      ;;
    --claims)
      CLAIMS=1
      shift
      ;;
    --verbose)
      VERBOSE=1
      shift
      ;;
    --once)
      ONCE=1
      shift
      ;;
    *) usage ;;
  esac
done

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GISARD_ROOT=${GISARD_ROOT:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
GISARD_BIN=${GISARD_BIN:-${GISARD_ROOT}/bin/campaign}
DASHBOARD_BIN=${DASHBOARD_BIN:-${GISARD_ROOT}/bin/campaign_dashboard}

if ((WATCH && ! ONCE)); then
  if [[ -t 0 && -t 1 && -x $DASHBOARD_BIN ]] && ((CLAIMS == 0 && VERBOSE == 0 && EVENTS == 10)); then
    exec "$DASHBOARD_BIN" "$CAMPAIGN" --refresh "$WATCH_SECONDS"
  fi
  child_args=(--once --events "$EVENTS")
  ((CLAIMS)) && child_args+=(--claims)
  ((VERBOSE)) && child_args+=(--verbose)
  trap 'printf "\n"; exit 0' INT TERM
  while true; do
    if [[ -t 1 ]]; then
      printf '\033[H\033[2J'
    fi
    "$0" "$CAMPAIGN" "${child_args[@]}"
    printf '\nnext refresh in %ss — Ctrl-C to stop\n' "$WATCH_SECONDS"
    sleep "$WATCH_SECONDS"
  done
fi

if [[ $CAMPAIGN != /* ]]; then
  CAMPAIGN=$(cd -- "$(dirname -- "$CAMPAIGN")" && pwd)/$(basename -- "$CAMPAIGN")
fi
LAUNCH_FILE=$CAMPAIGN/launch.env
MANIFEST_FILE=$CAMPAIGN/manifest.json

[[ -f $MANIFEST_FILE ]] || {
  echo "status: missing $MANIFEST_FILE" >&2
  exit 1
}
[[ -f $LAUNCH_FILE ]] || {
  echo "status: missing $LAUNCH_FILE; the launcher may have failed before submission" >&2
  exit 1
}
[[ -x $GISARD_BIN ]] || {
  echo "status: missing campaign binary: $GISARD_BIN" >&2
  exit 1
}
command -v squeue >/dev/null 2>&1 || {
  echo "status: squeue is unavailable; run this command on a LiDO gateway" >&2
  exit 1
}

source "$LAUNCH_FILE"
CACHE_KEY=$(printf '%s' "$CAMPAIGN" | sha256sum | cut -c1-16)
STATUS_CACHE=${TMPDIR:-/tmp}/gisard-status-${UID}-${CACHE_KEY}.json
PROBE_JOB_ID=${PROBE_JOB_ID:-}
MEMORY_JOB_IDS=${MEMORY_JOB_IDS:-}
SUBMIT_JOB_ID=${SUBMIT_JOB_ID:-}
CAMPAIGN_JOB_IDS=${CAMPAIGN_JOB_IDS:-}
POLISH_JOB_ID=${POLISH_JOB_ID:-}
HARVEST_JOB_ID=${HARVEST_JOB_ID:-}
SMOKE_MODE=${SMOKE_MODE:-0}
RESERVATION_NAME=${RESERVATION_NAME:-}
SUBMISSION_FILE=${SUBMISSION_FILE:-${CAMPAIGN}/submission/solution.txt}

json_u64() {
  local key=$1
  awk -F: -v key="\"${key}\"" '$1 ~ key {gsub(/[ ,]/, "", $2); print $2; exit}' "$MANIFEST_FILE"
}

duration() {
  local total=${1:-0}
  [[ $total =~ ^[0-9]+$ ]] || {
    printf '%s' "$total"
    return
  }
  printf '%dh%02dm%02ds' "$((total / 3600))" "$(((total % 3600) / 60))" "$((total % 60))"
}

append_detail() {
  if [[ -n $JOB_DETAIL ]]; then
    JOB_DETAIL="$JOB_DETAIL; $1"
  else
    JOB_DETAIL=$1
  fi
}

inspect_jobs() {
  local ids=$1
  local planned=$2
  local id rows first state elapsed left start reason account limit
  local has_active=0
  local has_running=0
  local has_pending=0
  local has_failed=0
  local has_unknown=0
  local count=0
  JOB_STATE=future
  JOB_DETAIL="not submitted"
  JOB_EXPECT=$planned
  [[ -n $ids ]] || return

  JOB_DETAIL=
  for id in ${ids//,/ }; do
    count=$((count + 1))
    rows=$(squeue -h -j "$id" -o "%T|%M|%L|%S|%R" 2>/dev/null)
    if [[ -n $rows ]]; then
      has_active=1
      first=$(printf '%s\n' "$rows" | head -n 1)
      IFS='|' read -r state elapsed left start reason <<EOF
$first
EOF
      case $state in
        RUNNING|COMPLETING)
          has_running=1
          ;;
        PENDING|CONFIGURING)
          has_pending=1
          ;;
      esac
      append_detail "$id $state, elapsed $elapsed, time-left $left, start ${start:-N/A}, ${reason:-no reason}"
      continue
    fi

    account=
    if command -v sacct >/dev/null 2>&1; then
      account=$(sacct -n -P -j "$id" \
        --format=JobIDRaw,State,Elapsed,Timelimit 2>/dev/null |
        awk -F'|' -v id="$id" '$1 == id {print; exit}')
    fi
    if [[ -z $account ]]; then
      has_unknown=1
      append_detail "$id absent from squeue; accounting not available yet"
      continue
    fi
    IFS='|' read -r _ state elapsed limit <<EOF
$account
EOF
    case $state in
      COMPLETED*)
        append_detail "$id COMPLETED in $elapsed"
        ;;
      FAILED*|CANCELLED*|TIMEOUT*|OUT_OF_MEMORY*|NODE_FAIL*|BOOT_FAIL*|DEADLINE*)
        has_failed=1
        append_detail "$id $state after $elapsed"
        ;;
      PENDING*|RUNNING*|CONFIGURING*|COMPLETING*|SUSPENDED*)
        has_active=1
        append_detail "$id $state, elapsed $elapsed, limit $limit"
        ;;
      *)
        has_unknown=1
        append_detail "$id $state, elapsed $elapsed, limit $limit"
        ;;
    esac
  done

  if ((has_failed)); then
    JOB_STATE=failed
    JOB_EXPECT="stopped; inspect the matching logs and sacct entry"
  elif ((has_active)); then
    JOB_STATE=active
    if ((has_running)); then
      JOB_EXPECT="running now; Slurm time-left is shown above; $planned"
    elif ((has_pending)); then
      JOB_EXPECT="queue wait is unknown when start is N/A; once running, $planned"
    else
      JOB_EXPECT=$planned
    fi
  elif ((has_unknown)); then
    JOB_STATE=unknown
    JOB_EXPECT="waiting for Slurm accounting; $planned"
  elif ((count > 0)); then
    JOB_STATE=passed
    JOB_EXPECT="finished"
  fi
}

PROBE_PLAN="limit 4h; normally calibration finishes earlier"
MEMORY_PLAN="one exact 5-worker x 4-chain check; limit 4h; builds five finest-density matrices and runs 60s of SA"
GATE_PLAN="limit 10m; normally only a few seconds"
SEARCH_PLAN="workers fill the usable window; promotions first, fresh 15m trials in the tail"
POLISH_PLAN="limit 1h; exact two-antenna MIP windows, globally rescored"
HARVEST_PLAN="limit 45m; exact validation may finish earlier"

inspect_jobs "$PROBE_JOB_ID" "$PROBE_PLAN"
PROBE_STATE=$JOB_STATE
PROBE_DETAIL=$JOB_DETAIL
PROBE_EXPECT=$JOB_EXPECT

inspect_jobs "$MEMORY_JOB_IDS" "$MEMORY_PLAN"
MEMORY_STATE=$JOB_STATE
MEMORY_DETAIL=$JOB_DETAIL
MEMORY_EXPECT=$JOB_EXPECT

inspect_jobs "$SUBMIT_JOB_ID" "$GATE_PLAN"
GATE_STATE=$JOB_STATE
GATE_DETAIL=$JOB_DETAIL
GATE_EXPECT=$JOB_EXPECT

inspect_jobs "$CAMPAIGN_JOB_IDS" "$SEARCH_PLAN"
SEARCH_STATE=$JOB_STATE
SEARCH_DETAIL=$JOB_DETAIL
SEARCH_EXPECT=$JOB_EXPECT

inspect_jobs "$POLISH_JOB_ID" "$POLISH_PLAN"
POLISH_STATE=$JOB_STATE
POLISH_DETAIL=$JOB_DETAIL
POLISH_EXPECT=$JOB_EXPECT

inspect_jobs "$HARVEST_JOB_ID" "$HARVEST_PLAN"
HARVEST_STATE=$JOB_STATE
HARVEST_DETAIL=$JOB_DETAIL
HARVEST_EXPECT=$JOB_EXPECT

[[ -n $MEMORY_JOB_IDS ]] && PROBE_STATE=passed
if [[ -f $CAMPAIGN/sa-portfolio.tsv ]]; then
  EXPECTED_MARKERS=$(awk 'NR > 1 {n++} END {print n + 0}' "$CAMPAIGN/sa-portfolio.tsv")
else
  EXPECTED_MARKERS=1
fi
MARKERS=0
if [[ -d $CAMPAIGN/memory-checks && -n $PROBE_JOB_ID ]]; then
  MARKERS=$(find "$CAMPAIGN/memory-checks" -maxdepth 1 -type f \
    -name "*-${PROBE_JOB_ID}.ok" -print 2>/dev/null | wc -l)
fi
if ((EXPECTED_MARKERS > 0 && MARKERS == EXPECTED_MARKERS)); then
  MEMORY_STATE=passed
  MEMORY_DETAIL="$MARKERS/$EXPECTED_MARKERS memory guards published success markers"
  MEMORY_EXPECT="finished"
else
  MEMORY_DETAIL="$MEMORY_DETAIL; markers $MARKERS/$EXPECTED_MARKERS"
fi
[[ -n $CAMPAIGN_JOB_IDS ]] && GATE_STATE=passed
if [[ -s $SUBMISSION_FILE ]]; then
  HARVEST_STATE=passed
  HARVEST_DETAIL="validated solution exists at $SUBMISSION_FILE"
  HARVEST_EXPECT="finished"
fi

DURATION_SECONDS=$(json_u64 duration_seconds)
RESERVE_SECONDS=$(json_u64 reserve_seconds)
if [[ $SMOKE_MODE == 1 ]]; then
  MODE="smoke"
else
  MODE="full"
fi
if [[ -n $RESERVATION_NAME ]]; then
  QUEUE="reservation $RESERVATION_NAME"
else
  QUEUE="normal nodes"
fi

printf 'GISard campaign dashboard  %s\n' "$(date --iso-8601=seconds)"
echo "campaign  $CAMPAIGN"
echo "mode      $MODE; $QUEUE"
if [[ -s $CAMPAIGN/sa-densities.tsv ]]; then
  DENSITIES=$(paste -sd, "$CAMPAIGN/sa-densities.tsv")
  echo "spacing   $DENSITIES"
else
  echo "spacing   waiting for probe"
fi
if [[ ${DURATION_SECONDS:-0} =~ ^[1-9][0-9]*$ ]]; then
  echo "clock     relative $(duration "$DURATION_SECONDS"); starts with first worker; reserve $(duration "${RESERVE_SECONDS:-0}")"
else
  DEADLINE_UNIX=$(json_u64 deadline_unix)
  STOP_UNIX=$((DEADLINE_UNIX - ${RESERVE_SECONDS:-0}))
  NOW_UNIX=$(date +%s)
  echo "clock     work stops $(date -d "@$STOP_UNIX" --iso-8601=seconds); $(duration "$((STOP_UNIX > NOW_UNIX ? STOP_UNIX - NOW_UNIX : 0))") left; reserve $(duration "${RESERVE_SECONDS:-0}")"
fi

EXPECTED_NODES=0
if [[ -s $CAMPAIGN/sa-portfolio.tsv ]]; then
  EXPECTED_NODES=$(awk 'NR > 1 {sum += $3} END {print sum + 0}' "$CAMPAIGN/sa-portfolio.tsv")
fi
RUNNING_NODES=0
PENDING_NODES=0
OTHER_NODES=0
if [[ -n $CAMPAIGN_JOB_IDS ]]; then
  JOB_ROWS=$(squeue -r -h -j "$CAMPAIGN_JOB_IDS" -o "%T" 2>/dev/null || true)
  RUNNING_NODES=$(printf '%s\n' "$JOB_ROWS" | awk '$1 == "RUNNING" {n++} END {print n + 0}')
  PENDING_NODES=$(printf '%s\n' "$JOB_ROWS" | awk '$1 == "PENDING" {n++} END {print n + 0}')
  OTHER_NODES=$(printf '%s\n' "$JOB_ROWS" | awk '$1 != "" && $1 != "RUNNING" && $1 != "PENDING" {n++} END {print n + 0}')
fi
echo "cluster   ${RUNNING_NODES}/${EXPECTED_NODES} nodes running; ${PENDING_NODES} pending; ${OTHER_NODES} other"
echo

CURRENT_FOUND=0
BLOCKED=0
CURRENT_NAME=
CURRENT_EXPECT=

print_phase() {
  local number=$1
  local name=$2
  local state=$3
  local detail=$4
  local expected=$5
  local badge
  case $state in
    passed)
      badge=PASSED
      ;;
    failed)
      badge=FAILED
      if ((CURRENT_FOUND == 0)); then
        CURRENT_FOUND=1
        BLOCKED=1
        CURRENT_NAME=$name
        CURRENT_EXPECT=$expected
      fi
      ;;
    active|unknown)
      if ((CURRENT_FOUND == 0)); then
        badge=CURRENT
        CURRENT_FOUND=1
        CURRENT_NAME=$name
        CURRENT_EXPECT=$expected
      elif ((BLOCKED)); then
        badge=BLOCKED
      else
        badge=QUEUED
      fi
      ;;
    future)
      if ((CURRENT_FOUND == 0)); then
        badge=NEXT
        CURRENT_FOUND=1
        CURRENT_NAME=$name
        CURRENT_EXPECT=$expected
      elif ((BLOCKED)); then
        badge=BLOCKED
      else
        badge=LATER
      fi
      ;;
  esac
  if ((VERBOSE)); then
    printf '[%-7s] %s. %s\n' "$badge" "$number" "$name"
    echo "          $detail"
    echo "          expectation: $expected"
  else
    printf '  %-7s  %-30s %s\n' "$badge" "$name" "$detail"
  fi
}

print_phase 0 "initialization" passed "manifest and launch.env exist" "finished"
print_phase 1 "spacing calibration/probe" "$PROBE_STATE" "$PROBE_DETAIL" "$PROBE_EXPECT"
print_phase 2 "memory guards" "$MEMORY_STATE" "$MEMORY_DETAIL" "$MEMORY_EXPECT"
print_phase 3 "production submission gate" "$GATE_STATE" "$GATE_DETAIL" "$GATE_EXPECT"
print_phase 4 "SA production search" "$SEARCH_STATE" "$SEARCH_DETAIL" "$SEARCH_EXPECT"
print_phase 5 "exact window MIP polish" "$POLISH_STATE" "$POLISH_DETAIL" "$POLISH_EXPECT"
print_phase 6 "harvest and exact validation" "$HARVEST_STATE" "$HARVEST_DETAIL" "$HARVEST_EXPECT"

if [[ -z $CURRENT_NAME && $HARVEST_STATE == passed ]]; then
  echo
  echo "CURRENT: COMPLETE — all phases passed"
  echo "solution: $SUBMISSION_FILE"
else
  echo
  echo "CURRENT: $CURRENT_NAME"
  echo "EXPECTED: $CURRENT_EXPECT"
fi

echo
echo "Solver progress"
if ((CLAIMS)); then
  "$GISARD_BIN" status "$CAMPAIGN" --claims --cache "$STATUS_CACHE"
elif ((VERBOSE)); then
  "$GISARD_BIN" status "$CAMPAIGN" --cache "$STATUS_CACHE"
else
  "$GISARD_BIN" status "$CAMPAIGN" --compact --cache "$STATUS_CACHE"
fi

if ((EVENTS > 0)); then
  echo
  echo "Recent events"
  "$GISARD_BIN" events "$CAMPAIGN" --limit "$EVENTS"
fi
