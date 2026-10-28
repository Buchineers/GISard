#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: GIS_CUP_EVALUATOR_DIR=/path/to/evaluator $0 <dataset.geojson> <antennas.txt> <tau> <k> <output.txt>" >&2
    exit 2
}

[[ $# -eq 5 ]] || usage

evaluator_dir=${GIS_CUP_EVALUATOR_DIR:-}
[[ -n "$evaluator_dir" ]] || {
    echo "GIS_CUP_EVALUATOR_DIR must point to a checkout of the official evaluator" >&2
    exit 2
}

dataset=$1
antennas=$2
tau=$3
k=$4
output=$5

[[ -f "$dataset" ]] || { echo "dataset not found: $dataset" >&2; exit 2; }
[[ -f "$antennas" ]] || { echo "antenna file not found: $antennas" >&2; exit 2; }
[[ -f "$evaluator_dir/src/core/evaluation-engine.ts" ]] || {
    echo "not an official evaluator checkout: $evaluator_dir" >&2
    exit 2
}
[[ -x "$evaluator_dir/node_modules/.bin/vitest" ]] || {
    echo "official evaluator dependencies are missing; run 'pnpm install' in $evaluator_dir" >&2
    exit 2
}

node_runner=${GIS_CUP_NODE:-}
if [[ -z "$node_runner" && -x "$evaluator_dir/node/bin/node" ]]; then
    node_runner=$evaluator_dir/node/bin/node
fi
if [[ -z "$node_runner" ]]; then
    node_runner=$(command -v node || true)
fi
[[ -x "$node_runner" ]] || {
    echo "Node.js is missing; set GIS_CUP_NODE or install it at $evaluator_dir/node/bin/node" >&2
    exit 2
}

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
dataset=$(realpath -- "$dataset")
antennas=$(realpath -- "$antennas")
output_parent=$(dirname -- "$output")
mkdir -p -- "$output_parent"
output=$(realpath -m -- "$output")

runner=$(mktemp --tmpdir="$evaluator_dir/benchmarks" --suffix=.test.ts gisard-official-score.XXXXXX)
cp -- "$script_dir/score_solution.test.ts" "$runner"
trap 'rm -f -- "$runner"' EXIT

export GISARD_OFFICIAL_DATASET=$dataset
export GISARD_OFFICIAL_ANTENNAS=$antennas
export GISARD_OFFICIAL_TAU=$tau
export GISARD_OFFICIAL_K=$k
export GISARD_OFFICIAL_OUTPUT=$output

revision=$(git -C "$evaluator_dir" rev-parse HEAD 2>/dev/null || echo unknown)
echo "official evaluator: $revision"
echo "node: $($node_runner --version) ($node_runner)"
echo "dataset: $dataset"
echo "placement: tau=$tau k=$k antennas=$antennas"

runner_relative=benchmarks/$(basename -- "$runner")
(
    cd -- "$evaluator_dir"
    "$node_runner" node_modules/vitest/vitest.mjs run \
        --config vitest.benchmark.config.ts \
        "$runner_relative" \
        --reporter=verbose
)
