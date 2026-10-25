#!/usr/bin/env bash
# Compares this checkout's Criterion benchmarks with another revision's, on this machine: both
# are built, then run in turn (base, head, head, base, ... ROUNDS of each), each run into a
# Criterion directory of its own, and scripts/criterion-compare.py judges every benchmark
# against how far each side's own runs lie apart. Runs that alternate share whatever the machine
# was doing; one run of each, minutes apart, does not.
#
# Usage:
#   ./scripts/criterion-ab.sh <base revision> [Criterion arguments...]
#
# Environment: ROUNDS (default 2), THRESHOLD (percent, default 10), NTFS_NG_BENCH_RECORDS.
# Results: target/criterion-ab/<n>-<base|head>/, and a Markdown table on stdout.
set -euo pipefail

SCRIPT_PATH="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
REPO_ROOT="$(cd "$(dirname "$SCRIPT_PATH")/.." && pwd -P)"
cd "$REPO_ROOT"

BASE_REV="${1:?usage: criterion-ab.sh <base revision> [Criterion arguments...]}"
shift
ROUNDS="${ROUNDS:-2}"
THRESHOLD="${THRESHOLD:-10}"
TARGET="${CARGO_TARGET_DIR:-target}"
OUT="$REPO_ROOT/$TARGET/criterion-ab"
WORKTREE="$REPO_ROOT/$TARGET/criterion-ab-base"
PYTHON="$(command -v python3 || command -v python)"

# The executables of every benchmark target `cargo bench` runs, built in `$1`.
bench_executables() {
    (cd "$1" && cargo bench --all-features --no-run --message-format=json) | "$PYTHON" -c '
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("executable") \
            and "bench" in message["target"]["kind"]:
        print(message["executable"])
'
}

rm -rf "$OUT"
mkdir -p "$OUT"
git worktree remove --force "$WORKTREE" 2>/dev/null || rm -rf "$WORKTREE"
git worktree add --detach --quiet "$WORKTREE" "$BASE_REV"
trap 'git worktree remove --force "$WORKTREE" 2>/dev/null || true' EXIT

echo "Building the base revision ($BASE_REV) and this checkout..." >&2
# (No `mapfile`: bash 3.2, on macOS, lacks it.)
BASE=()
while IFS= read -r exe; do BASE+=("$exe"); done < <(CARGO_TARGET_DIR="$WORKTREE/target" bench_executables "$WORKTREE")
HEAD=()
while IFS= read -r exe; do HEAD+=("$exe"); done < <(bench_executables "$REPO_ROOT")
if [[ ${#BASE[@]} -eq 0 || ${#HEAD[@]} -eq 0 ]]; then
    echo "No benchmarks to compare (base: ${#BASE[@]} targets, head: ${#HEAD[@]})." >&2
    exit 1
fi

run=0
for ((round = 0; round < ROUNDS; round++)); do
    if ((round % 2 == 0)); then order=(base head); else order=(head base); fi
    for side in "${order[@]}"; do
        run=$((run + 1))
        echo "Run $run: $side" >&2
        if [[ "$side" == base ]]; then dir="$WORKTREE"; exes=("${BASE[@]}"); else dir="$REPO_ROOT"; exes=("${HEAD[@]}"); fi
        for exe in "${exes[@]}"; do
            (cd "$dir" && CRITERION_HOME="$OUT/$run-$side" "$exe" --bench --noplot "$@" > /dev/null)
        done
    done
done

"$PYTHON" "$REPO_ROOT/scripts/criterion-compare.py" --runs "$OUT" --threshold "$THRESHOLD"
