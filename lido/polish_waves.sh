#!/bin/bash

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[$(date -u +%H:%M:%S)] $*"; }

usage() {
  cat <<'EOF'
usage: polish_waves.sh --campaign <dir> [options]

  --campaign <dir>     campaign directory (required)
  --scenarios <a,b>    scenario ids [all in the manifest]
  --nodes <n>          concurrent nodes [6]
  --until <HH:MM>      UTC cutoff for starting new waves [none]
  --reserve-min <n>    minutes to leave free before --until [45]
  --partition <list>   Slurm partition(s) [long,med]
  --antennas <n>       antennas freed per window [2]
  --spacing <units>    candidate spacing [8]
  --centres <mode>     antenna | open | both [antenna]
                       `open` anchors on an unserviced building and frees its
                       nearest antenna, so at sparse k the disc must stretch
                       hundreds of units to reach that antenna and every window
                       burns the full oracle limit. `antenna` centres the disc
                       on the antenna itself, which stays small and solvable.
  --time <walltime>    per-wave array walltime [04:00:00]
  --poll-min <n>       minutes between checks when stalled [10]
  --variants <list>    ladder of "timelimit:radius:antennas" climbed whenever a
                       wave certifies everything without finding anything. Each
                       rung is a strictly stronger claim than the last, so the
                       sweep is anytime: more time buys a bigger window rather
                       than a rerun of one already exhausted.
                       [300:60:1,900:60:1,900:120:1,900:200:1,1800:120:2]

                       Radius escalates before antenna count on purpose. Widening
                       the disc grows the model quadratically and keeps one
                       antenna centred in it; freeing a second antenna grows it
                       combinatorially AND stretches the disc to span both, which
                       at sparse k is what made windows unsolvable.
  --dry-run            print the first wave's plan, submit nothing

Run detached:
  nohup ./lido/polish_waves.sh --campaign ... --until 14:00 > logs/waves.log 2>&1 &
EOF
}

CAMPAIGN=""; SCENARIOS=""; NODES=6; UNTIL=""; RESERVE_MIN=45
PARTITION=long,med; ANTENNAS=2; SPACING=8; WALLTIME=04:00:00
POLL_MIN=10; VARIANTS="300:60:1,900:60:1,900:120:1,900:200:1,1800:120:2"
CENTRES=antenna; DRY_RUN=0

while (($#)); do
  case $1 in
    --campaign) CAMPAIGN=$2; shift 2;;
    --scenarios) SCENARIOS=$2; shift 2;;
    --nodes) NODES=$2; shift 2;;
    --until) UNTIL=$2; shift 2;;
    --reserve-min) RESERVE_MIN=$2; shift 2;;
    --partition) PARTITION=$2; shift 2;;
    --antennas) ANTENNAS=$2; shift 2;;
    --spacing) SPACING=$2; shift 2;;
    --centres) CENTRES=$2; shift 2;;
    --time) WALLTIME=$2; shift 2;;
    --poll-min) POLL_MIN=$2; shift 2;;
    --variants) VARIANTS=$2; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    -h|--help) usage; exit 0;;
    *) die "unknown argument: $1";;
  esac
done

[[ -n $CAMPAIGN ]] || { usage; die "--campaign is required"; }
[[ -d $CAMPAIGN ]] || die "no such campaign: $CAMPAIGN"

GISARD_ROOT=${GISARD_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
GISARD_BIN=${GISARD_BIN:-$GISARD_ROOT/bin/campaign}
[[ -x $GISARD_BIN ]] || die "no campaign binary at $GISARD_BIN"

STOP_UNIX=0
if [[ -n $UNTIL ]]; then
  STOP_UNIX=$(date -u -d "today $UNTIL" +%s 2>/dev/null) || die "bad --until: $UNTIL"
  (( STOP_UNIX < $(date -u +%s) )) && STOP_UNIX=$((STOP_UNIX + 86400))
fi

IFS=',' read -r -a VARIANT_LIST <<< "$VARIANTS"

incumbent_state() {
  ( cd "$CAMPAIGN/best" 2>/dev/null || return 0
    for s in */; do
      top=$(ls -1 "${s}cand" 2>/dev/null | sort -r | head -1)
      printf '%s:%s\n' "${s%/}" "${top:-none}"
    done ) | sha256sum | cut -d' ' -f1
}

wait_for_job() {
  while squeue -h -j "$1" -o "%i" 2>/dev/null | grep -q .; do sleep 20; done
}

usable_seconds() {
  ((STOP_UNIX == 0)) && { echo 999999; return; }
  echo $(( STOP_UNIX - RESERVE_MIN * 60 - $(date -u +%s) ))
}

log "campaign $CAMPAIGN"
log "nodes $NODES, antennas $ANTENNAS, centres $CENTRES, spacing $SPACING, partition $PARTITION"
log "variant ladder: ${VARIANT_LIST[*]}"
((STOP_UNIX > 0)) && log "cutoff $(date -u -d "@$STOP_UNIX" +'%a %H:%M UTC') minus ${RESERVE_MIN}m reserve"

TOTAL=0; wave=0; variant=0; FAILURES=0
LAST_STATE=$(incumbent_state)

while :; do
  remaining=$(usable_seconds)
  if ((remaining <= 0)); then
    log "cutoff reached; stopping. total gained: $TOTAL over $wave wave(s)"
    break
  fi

  wave=$((wave + 1))
  IFS=':' read -r TIME_LIMIT RADIUS RUNG_ANTENNAS <<< "${VARIANT_LIST[$variant]}"
  RUNG_ANTENNAS=${RUNG_ANTENNAS:-$ANTENNAS}
  DIR="$CAMPAIGN/polish-wave$(printf '%02d' "$wave")"

  log "wave $wave: rung $((variant + 1))/${#VARIANT_LIST[@]} — oracle ${TIME_LIMIT}s, radius $RADIUS, antennas $RUNG_ANTENNAS, $((remaining / 60))m left"
  mkdir -p "$DIR"

  ARGS=(--campaign "$CAMPAIGN" --shard-dir "$DIR" --nodes "$NODES"
        --partition "$PARTITION" --antennas "$RUNG_ANTENNAS" --radius "$RADIUS"
        --time-limit "$TIME_LIMIT" --spacing "$SPACING" --time "$WALLTIME"
        --centres "$CENTRES")
  [[ -n $SCENARIOS ]] && ARGS+=(--scenarios "$SCENARIOS")
  ((DRY_RUN)) && ARGS+=(--dry-run)

  GISARD_BIN="$GISARD_BIN" GISARD_ROOT="$GISARD_ROOT" \
    "$GISARD_ROOT/lido/launch_polish_fanout.sh" "${ARGS[@]}" 2>&1 | tee "$DIR/launch.out"

  ((DRY_RUN)) && { log "dry run complete"; break; }

  [[ -f $DIR/launch.env ]] || { log "wave $wave: no launch.env, retrying after ${POLL_MIN}m"; sleep $((POLL_MIN * 60)); continue; }
  # shellcheck disable=SC1090
  source "$DIR/launch.env"
  log "wave $wave: array $POLISH_ARRAY_JOB_ID, merge $POLISH_MERGE_JOB_ID"
  wait_for_job "$POLISH_ARRAY_JOB_ID"
  wait_for_job "$POLISH_MERGE_JOB_ID"

  PRODUCED=$(find "$DIR" -name '*.json' 2>/dev/null | wc -l)
  if ((PRODUCED == 0)); then
    FAILURES=$((FAILURES + 1))
    log "wave $wave: NO shard files produced — the sweep failed, not converged"
    sed -n '1,3p' "$GISARD_ROOT/logs/polish-shard-${POLISH_ARRAY_JOB_ID}_0.err" 2>/dev/null | sed 's/^/    /'
    if ((FAILURES >= 2)); then
      log "two consecutive failed waves; stopping rather than burning nodes"
      break
    fi
    log "retrying after ${POLL_MIN}m"
    sleep $((POLL_MIN * 60))
    continue
  fi
  FAILURES=0

  MERGE_LOG="$GISARD_ROOT/logs/polish-merge-$POLISH_MERGE_JOB_ID.out"
  GAIN=$(grep -oE 'total gained: -?[0-9]+' "$MERGE_LOG" 2>/dev/null | tail -1 | grep -oE '\-?[0-9]+' || true)
  GAIN=${GAIN:-0}
  log "wave $wave: $PRODUCED shard files"
  TOTAL=$((TOTAL + GAIN))
  log "wave $wave: gained $GAIN (cumulative $TOTAL)"
  grep -E '^\S+ +[0-9]+ -> ' "$MERGE_LOG" 2>/dev/null | sed 's/^/    /' || true

  if ((GAIN > 0)); then
    variant=0
    LAST_STATE=$(incumbent_state)
    continue
  fi

  if ((variant + 1 < ${#VARIANT_LIST[@]})); then
    variant=$((variant + 1))
    log "wave $wave certified clean with no gain; climbing to rung $((variant + 1)): ${VARIANT_LIST[$variant]}"
    continue
  fi

  log "top of the ladder reached and nothing found — the placements are locally optimal out to the widest window we can solve. waiting for the incumbent to move (poll ${POLL_MIN}m)"
  while :; do
    remaining=$(usable_seconds)
    ((remaining <= 0)) && break
    sleep $((POLL_MIN * 60))
    STATE=$(incumbent_state)
    if [[ $STATE != "$LAST_STATE" ]]; then
      log "incumbent changed; restarting the ladder"
      LAST_STATE=$STATE
      variant=0
      break
    fi
    log "incumbent unchanged, $((remaining / 60))m left"
  done
done

log "final: $TOTAL buildings gained over $wave wave(s)"
