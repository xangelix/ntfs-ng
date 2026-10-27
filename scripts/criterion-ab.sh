#!/usr/bin/env bash
# Compares this checkout's Criterion benchmarks with another revision's, on this machine: both
# are built, then run in turn, group by group: each Criterion group (`index`, `scan`, ...) of
# the base, then of the head, then the head again and the base again, ROUNDS of each, before
# the next group. Every run goes into a Criterion directory of its own, and
# scripts/criterion-compare.py judges every benchmark against how far each side's own runs lie
# apart. Runs that alternate within a group lie seconds apart and share whatever the machine
# was doing; runs of whole suites lie minutes apart, and a slow minute lands on one side.
#
# Usage:
#   ./scripts/criterion-ab.sh <base revision> [Criterion options...]
#
# Environment: ROUNDS (default 2), THRESHOLD (percent, default 10), FILTER (a regular
# expression: only the benchmarks it matches), NTFS_NG_BENCH_RECORDS.
# Results: target/criterion-ab/<n>-<base|head>/, and a Markdown table on stdout.
set -euo pipefail

SCRIPT_PATH="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
REPO_ROOT="$(cd "$(dirname "$SCRIPT_PATH")/.." && pwd -P)"
cd "$REPO_ROOT"

BASE_REV="${1:?usage: criterion-ab.sh <base revision> [Criterion options...]}"
shift
ROUNDS="${ROUNDS:-2}"
THRESHOLD="${THRESHOLD:-10}"
FILTER="${FILTER:-}"
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

# Every benchmark of one side's executables, as "<side>\t<executable>\t<benchmark>" lines.
list_benchmarks() {
    local side="$1" dir="$2" exe
    shift 2
    for exe in "$@"; do
        (cd "$dir" && "$exe" --bench --list < /dev/null) | sed -n 's/: benchmark$//p' \
            | while IFS= read -r bench; do printf '%s\t%s\t%s\n' "$side" "$exe" "$bench"; done
    done
}

# The runs, from the benchmarks listed: group by group, the sides taking turns within each
# (base, head, then head, base, ...). One "<run>\t<side>\t<executable>\t<filter>\t<group>" line
# each; the filter matches exactly the group's benchmarks in that executable.
schedule() {
    "$PYTHON" -c '
import re, sys
rounds, wanted = int(sys.argv[1]), re.compile(sys.argv[2])
groups = {}
for line in sys.stdin:
    side, exe, bench = line.rstrip("\n").split("\t")
    if wanted.search(bench):
        groups.setdefault(bench.split("/")[0], {}).setdefault(side, {}).setdefault(exe, []).append(bench)
escape = lambda bench: re.sub(r"([\\.+*?()|\[\]{}^$])", r"\\\1", bench)
for group, sides in groups.items():
    for round in range(rounds):
        for at, side in enumerate(("base", "head") if round % 2 == 0 else ("head", "base")):
            for exe, benches in sides.get(side, {}).items():
                exact = "^(?:" + "|".join(map(escape, benches)) + ")$"
                print(f"{2 * round + at + 1}\t{side}\t{exe}\t{exact}\t{group}")
' "$ROUNDS" "$FILTER"
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

{
    list_benchmarks head "$REPO_ROOT" "${HEAD[@]}"
    list_benchmarks base "$WORKTREE" "${BASE[@]}"
} > "$OUT/benchmarks.tsv"
schedule < "$OUT/benchmarks.tsv" > "$OUT/schedule.tsv"
total="$(wc -l < "$OUT/schedule.tsv" | tr -d ' ')"
if [[ "$total" -eq 0 ]]; then
    echo "No benchmarks to compare${FILTER:+ (FILTER: $FILTER)}." >&2
    exit 1
fi

done_runs=0
while IFS=$'\t' read -r run side exe filter group; do
    done_runs=$((done_runs + 1))
    echo "[$done_runs/$total] $group: run $run, $side" >&2
    if [[ "$side" == base ]]; then dir="$WORKTREE"; else dir="$REPO_ROOT"; fi
    (cd "$dir" && CRITERION_HOME="$OUT/$run-$side" "$exe" --bench --noplot "$@" "$filter" < /dev/null > /dev/null)
done < "$OUT/schedule.tsv"

"$PYTHON" "$REPO_ROOT/scripts/criterion-compare.py" --runs "$OUT" --threshold "$THRESHOLD"
