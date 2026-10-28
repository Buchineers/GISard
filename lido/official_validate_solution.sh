#!/bin/bash

set -euo pipefail

usage() {
  echo "usage: $0 <evaluator-checkout> <dataset.geojson> <solution.txt> [report.json]" >&2
  exit 2
}

[[ $# -ge 3 && $# -le 4 ]] || usage

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
EVALUATOR=$(cd -- "$1" && pwd)
DATASET=$(realpath "$2")
SOLUTION=$(realpath "$3")
REPORT=$(realpath -m "${4:-${SOLUTION}.official.json}")
VITEST=$EVALUATOR/node_modules/.bin/vitest

[[ -f $DATASET ]] || { echo "dataset not found: $DATASET" >&2; exit 1; }
[[ -f $SOLUTION ]] || { echo "solution not found: $SOLUTION" >&2; exit 1; }
[[ -x $VITEST ]] || { echo "install the evaluator dependencies first: $VITEST is missing" >&2; exit 1; }
[[ -f $EVALUATOR/vitest.benchmark.config.ts ]] || { echo "not an evaluator checkout: $EVALUATOR" >&2; exit 1; }

TEST_FILE=$(mktemp "$EVALUATOR/benchmarks/gisard-final-XXXXXX.test.ts")
cleanup() {
  rm -f -- "$TEST_FILE"
}
trap cleanup EXIT
cp -- "$SCRIPT_DIR/official_final.test.ts" "$TEST_FILE"

echo "official evaluator: $EVALUATOR"
echo "dataset:            $DATASET"
echo "solution:           $SOLUTION"
echo "report:             $REPORT"
echo "dataset SHA-256:    $(sha256sum "$DATASET" | awk '{print $1}')"
echo "solution SHA-256:   $(sha256sum "$SOLUTION" | awk '{print $1}')"

cd "$EVALUATOR"
GISARD_OFFICIAL_DATASET=$DATASET \
GISARD_OFFICIAL_SOLUTION=$SOLUTION \
GISARD_OFFICIAL_REPORT=$REPORT \
  "$VITEST" run \
    --config vitest.benchmark.config.ts \
    --testTimeout 14400000 \
    "$TEST_FILE" \
    --reporter=verbose

echo "official validation passed: $REPORT"
