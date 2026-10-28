#!/bin/bash

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: launch_polish_fanout.sh --campaign <dir> [options]

  --campaign <dir>     campaign directory (required)
  --scenarios <a,b>    only these scenario ids [all in the manifest]
  --nodes <n>          concurrent nodes; also the shard count when >= 9 [9]
  --partition <list>   Slurm partition(s) [long,med]
                       NOTE: cstd01 nodes are in `long`, cstd02 are only in
                       `med`/`short`. A `long`-only job silently strands every
                       cstd02 node it was given.
  --shard-dir <dir>    where verdicts land [<campaign>/polish-shards]
                       a second wave needs its own: it certifies against the
                       incumbent the first wave produced, and mixing the two
                       is exactly what the fingerprint check rejects
  --antennas <n>       antennas freed per window [2]
                       3 measured WORSE, not better: at radius 60 the disc is
                       already capped, so the extra freed antenna adds a
                       variable without adding territory — and the incumbent's
                       third antenna is often absent from the repair pool, so
                       "leave it put" is not even representable and the wider
                       window loses improvements the narrow one finds
  --radius <units>     window radius [60]
  --windows <n>        windows per scenario [exhaustive: one per antenna]
  --time-limit <s>     oracle seconds per window [60]
  --spacing <units>    candidate spacing for the repair pool [8]
  --centres <mode>     antenna | open | both [both]
  --time <walltime>    array walltime [02:00:00]
  --reservation <name> Slurm reservation [gis-cup-2026]
  --no-reservation     submit without a reservation
  --dry-run            write the plan and print the sbatch commands, submit nothing

To free nodes first (the SA array keeps its incumbents either way — they are
already checkpointed under <campaign>/best):

  scancel 40144071_[101-200]      # half the array
  scancel 40144071                # all of it

EOF
}

CAMPAIGN=""
SCENARIOS=""
SHARD_DIR_OVERRIDE=""
NODES=9
ANTENNAS=2
RADIUS=60
WINDOWS=""
TIME_LIMIT=60
SPACING=8
CENTRES=both
WALLTIME=02:00:00
RESERVATION=gis-cup-2026
PARTITION=long,med
DRY_RUN=0

while (($#)); do
  case $1 in
    --campaign) (($# >= 2)) || die "--campaign needs a value"; CAMPAIGN=$2; shift 2;;
    --scenarios) (($# >= 2)) || die "--scenarios needs a value"; SCENARIOS=$2; shift 2;;
    --shard-dir) (($# >= 2)) || die "--shard-dir needs a value"; SHARD_DIR_OVERRIDE=$2; shift 2;;
    --nodes) (($# >= 2)) || die "--nodes needs a value"; NODES=$2; shift 2;;
    --antennas) (($# >= 2)) || die "--antennas needs a value"; ANTENNAS=$2; shift 2;;
    --radius) (($# >= 2)) || die "--radius needs a value"; RADIUS=$2; shift 2;;
    --windows) (($# >= 2)) || die "--windows needs a value"; WINDOWS=$2; shift 2;;
    --time-limit) (($# >= 2)) || die "--time-limit needs a value"; TIME_LIMIT=$2; shift 2;;
    --spacing) (($# >= 2)) || die "--spacing needs a value"; SPACING=$2; shift 2;;
    --centres) (($# >= 2)) || die "--centres needs a value"; CENTRES=$2; shift 2;;
    --time) (($# >= 2)) || die "--time needs a value"; WALLTIME=$2; shift 2;;
    --reservation) (($# >= 2)) || die "--reservation needs a value"; RESERVATION=$2; shift 2;;
    --partition) (($# >= 2)) || die "--partition needs a value"; PARTITION=$2; shift 2;;
    --no-reservation) RESERVATION=""; shift;;
    --dry-run) DRY_RUN=1; shift;;
    -h|--help) usage; exit 0;;
    *) die "unknown argument: $1";;
  esac
done

[[ -n $CAMPAIGN ]] || { usage; die "--campaign is required"; }
[[ -d $CAMPAIGN ]] || die "no such campaign: $CAMPAIGN"
[[ -f $CAMPAIGN/manifest.json ]] || die "no manifest.json in $CAMPAIGN"
[[ $NODES =~ ^[0-9]+$ ]] && ((NODES > 0)) || die "invalid --nodes: $NODES"
[[ $CENTRES =~ ^(antenna|open|both)$ ]] || die "invalid --centres: $CENTRES"

GISARD_ROOT=${GISARD_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
GISARD_BIN=${GISARD_BIN:-$GISARD_ROOT/bin/campaign}
[[ -x $GISARD_BIN ]] || die "no campaign binary at $GISARD_BIN (run make sync)"

SHARD_DIR=${SHARD_DIR_OVERRIDE:-$CAMPAIGN/polish-shards}
PLAN=$SHARD_DIR/shards.plan
mkdir -p "$SHARD_DIR" "$GISARD_ROOT/logs"

if compgen -G "$SHARD_DIR/*.json" > /dev/null; then
  die "$SHARD_DIR already holds shard files; move them aside first"
fi

python3 - "$CAMPAIGN/manifest.json" "$NODES" "$SCENARIOS" > "$PLAN" <<'PY'
import json, sys

manifest = json.load(open(sys.argv[1]))
nodes = int(sys.argv[2])
wanted = [s for s in sys.argv[3].split(",") if s] if len(sys.argv) > 3 else []
scenarios = manifest["scenarios"]
if wanted:
    known = {s["id"] for s in scenarios}
    missing = [s for s in wanted if s not in known]
    if missing:
        sys.exit("no such scenario(s) in manifest: %s" % ", ".join(missing))
    scenarios = [s for s in scenarios if s["id"] in wanted]

total_k = sum(s["k"] for s in scenarios) or len(scenarios)
budget = max(nodes, len(scenarios))

shards = {}
for s in scenarios:
    shards[s["id"]] = max(1, round(budget * s["k"] / total_k))

lines = []
for s in scenarios:
    count = shards[s["id"]]
    for index in range(count):
        lines.append("%s %d %d" % (s["id"], index, count))

sys.stdout.write("\n".join(lines) + "\n")
sys.stderr.write("plan: %d array tasks over %d nodes\n" % (len(lines), nodes))
for s in scenarios:
    sys.stderr.write("  %-14s k=%-4d %3d shards\n" % (s["id"], s["k"], shards[s["id"]]))
PY

TASKS=$(wc -l < "$PLAN")
((TASKS > 0)) || die "empty plan"

echo
echo "campaign:    $CAMPAIGN"
echo "shards:      $SHARD_DIR  ($TASKS array tasks, max $NODES concurrent)"
echo "window:      radius $RADIUS, $ANTENNAS antennas, centres $CENTRES, spacing $SPACING"
echo "windows:     ${WINDOWS:-exhaustive (one per antenna)}"
echo "oracle:      ${TIME_LIMIT}s per window"
echo "walltime:    $WALLTIME"
echo "partition:   $PARTITION"
echo "reservation: ${RESERVATION:-none}"
echo

EXPORT="ALL,GISARD_ROOT=$GISARD_ROOT,GISARD_BIN=$GISARD_BIN,GISARD_CAMPAIGN=$CAMPAIGN"
EXPORT="$EXPORT,GISARD_SHARDS=$SHARD_DIR,GISARD_PLAN=$PLAN,GISARD_RADIUS=$RADIUS"
EXPORT="$EXPORT,GISARD_ANTENNAS=$ANTENNAS,GISARD_TIME_LIMIT=$TIME_LIMIT"
EXPORT="$EXPORT,GISARD_SPACING=$SPACING,GISARD_CENTRES=$CENTRES,GISARD_WINDOWS=$WINDOWS"

RES_FLAG=()
[[ -n $RESERVATION ]] && RES_FLAG=(--reservation "$RESERVATION")

ARRAY_CMD=(sbatch --parsable
  --array="0-$((TASKS - 1))%$NODES"
  --partition="$PARTITION"
  --time="$WALLTIME"
  --export="$EXPORT"
  "${RES_FLAG[@]}"
  "$GISARD_ROOT/lido/polish_shards.sbatch")

if ((DRY_RUN)); then
  echo "would run: ${ARRAY_CMD[*]}"
  echo "then:      sbatch --dependency=afterany:<array> ... lido/polish_merge.sbatch"
  echo
  echo "plan written to $PLAN"
  exit 0
fi

cd "$GISARD_ROOT"
ARRAY_ID=$("${ARRAY_CMD[@]}")
echo "shard array: $ARRAY_ID"

MERGE_ID=$(sbatch --parsable \
  --partition="$PARTITION" \
  --dependency="afterany:$ARRAY_ID" \
  --export="$EXPORT" \
  "${RES_FLAG[@]}" \
  "$GISARD_ROOT/lido/polish_merge.sbatch")
echo "merge:       $MERGE_ID"

cat > "$SHARD_DIR/launch.env" <<EOF
POLISH_ARRAY_JOB_ID=$ARRAY_ID
POLISH_MERGE_JOB_ID=$MERGE_ID
POLISH_SHARD_DIR=$SHARD_DIR
POLISH_TASKS=$TASKS
POLISH_NODES=$NODES
POLISH_ANTENNAS=$ANTENNAS
POLISH_RADIUS=$RADIUS
POLISH_WINDOWS=$WINDOWS
POLISH_CENTRES=$CENTRES
POLISH_SPACING=$SPACING
POLISH_TIME_LIMIT=$TIME_LIMIT
EOF
echo "recorded:    $SHARD_DIR/launch.env"
