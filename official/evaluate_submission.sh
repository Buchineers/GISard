#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: GIS_CUP_EVALUATOR_DIR=/path/to/evaluator $0 <dataset.geojson> <input.txt> <official-output.txt>" >&2
    exit 2
}

[[ $# -eq 3 ]] || usage

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
dataset=$1
input=$2
output=$3

[[ -f "$dataset" ]] || { echo "dataset not found: $dataset" >&2; exit 2; }
[[ -f "$input" ]] || { echo "submission not found: $input" >&2; exit 2; }
[[ -n ${GIS_CUP_EVALUATOR_DIR:-} ]] || {
    echo "GIS_CUP_EVALUATOR_DIR must point to the official evaluator" >&2
    exit 2
}
[[ -x "$script_dir/score_solution.sh" ]] || {
    echo "missing scorer: $script_dir/score_solution.sh" >&2
    exit 2
}

dataset=$(realpath -- "$dataset")
input=$(realpath -- "$input")
output_parent=$(dirname -- "$output")
mkdir -p -- "$output_parent"
output=$(realpath -m -- "$output")
[[ "$input" != "$output" ]] || {
    echo "input and official output must be different files" >&2
    exit 2
}

mapfile -t lines <"$input"
(( ${#lines[@]} > 0 && ${#lines[@]} % 3 == 0 )) || {
    echo "submission must contain complete three-line blocks" >&2
    exit 2
}

work=$(mktemp -d --tmpdir="$output_parent" .gisard-official.XXXXXX)
cleanup() {
    rm -rf -- "$work"
}
trap cleanup EXIT

blocks=$((${#lines[@]} / 3))
pids=()
names=()
mkdir -p -- "$output.official-logs"
for ((i = 0; i < blocks; i++)); do
    offset=$((3 * i))
    parameters=${lines[offset]}
    coordinates=${lines[offset + 1]}
    if [[ ! $parameters =~ ^\([[:space:]]*([^,]+)[[:space:]]*,[[:space:]]*([0-9]+)[[:space:]]*\)$ ]]; then
        echo "block $((i + 1)): malformed parameter line: $parameters" >&2
        exit 2
    fi
    tau=${BASH_REMATCH[1]//[[:space:]]/}
    k=${BASH_REMATCH[2]}
    name="block-$((i + 1))-t${tau}-k${k}"
    antennas=$work/$name.antennas.txt
    result=$work/$name.result.txt
    log=$output.official-logs/$name.log
    printf '%s\n' "$coordinates" | sed -E 's/\)[[:space:]]*,[[:space:]]*\(/)\n(/g' >"$antennas"

    (
        "$script_dir/score_solution.sh" "$dataset" "$antennas" "$tau" "$k" "$result"
    ) >"$log" 2>&1 &
    pids+=("$!")
    names+=("$name")
done

failed=0
for ((i = 0; i < blocks; i++)); do
    if wait "${pids[i]}"; then
        score=$(sed -n '3p' "$work/${names[i]}.result.txt" | awk -F, '{ if ($0 == "") print 0; else print NF }')
        echo "official ${names[i]}: $score serviced"
    else
        echo "official ${names[i]} failed; inspect $output.official-logs/${names[i]}.log" >&2
        failed=1
    fi
done
((failed == 0)) || exit 1

temporary=$output.$$.tmp
for name in "${names[@]}"; do
    cat "$work/$name.result.txt"
done >"$temporary"
mv -- "$temporary" "$output"
echo "official submission ready: $output"
