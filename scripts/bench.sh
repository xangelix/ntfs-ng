#!/usr/bin/env bash
# Runs ntfs-ng's caller-shaped workloads (ntfs-bench) over synthetic corpora, and over any
# `$MFT` dumps or volume images given, and writes JSON lines plus a Markdown summary.
#
# Usage:
#   ./scripts/bench.sh [--records N] [--iterations N] [--threads N] [--output DIR]
#                      [--mft PATH]... [--image PATH]...
#
# Synthetic corpora (generated once, cached in ./corpora): N records of 1 KiB, and N/4 of
# 4 KiB. Real dumps never leave the machine: only their timings are written.
set -euo pipefail

SCRIPT_PATH="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
REPO_ROOT="$(cd "$(dirname "$SCRIPT_PATH")/.." && pwd -P)"
cd "$REPO_ROOT"

RECORDS=1000000
ITERATIONS=5
THREADS=0
OUTPUT="bench-results"
MFTS=()
IMAGES=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --records) RECORDS="$2"; shift 2 ;;
        --iterations) ITERATIONS="$2"; shift 2 ;;
        --threads) THREADS="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --mft) MFTS+=("$2"); shift 2 ;;
        --image) IMAGES+=("$2"); shift 2 ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done

mkdir -p corpora "$OUTPUT"
cargo build --release --features testing --example ntfs-bench
BIN="${CARGO_TARGET_DIR:-target}/release/examples/ntfs-bench"

SYN1="corpora/synthetic-${RECORDS}-1k.mft"
SYN4="corpora/synthetic-$((RECORDS / 4))-4k.mft"
[[ -f "$SYN1" ]] || "$BIN" generate "$SYN1" --records "$RECORDS"
[[ -f "$SYN4" ]] || "$BIN" generate "$SYN4" --records "$((RECORDS / 4))" --record-size 4096

RESULTS="$OUTPUT/results.jsonl"
: > "$RESULTS"
run() {
    local label="$1" input="$2" kind="$3"
    local flag=()
    [[ "$kind" == image ]] && flag=(--image)
    # Warm the page cache: the workloads measure parsing, not the disk.
    cat "$input" > /dev/null
    for workload in dirs du paths paths-raw inventory; do
        "$BIN" "$workload" "$input" ${flag[@]+"${flag[@]}"} --iterations "$ITERATIONS" --threads "$THREADS" \
            | sed "s/^{/{\"corpus\":\"$label\",/" >> "$RESULTS"
    done
}
run "synthetic-1k" "$SYN1" mft
run "synthetic-4k" "$SYN4" mft
# `${a[@]+"${a[@]}"}`: an empty array is "unbound" to bash 3.2 (macOS) under `set -u`.
for path in ${MFTS[@]+"${MFTS[@]}"}; do run "mft:$(basename "$(dirname "$path")")" "$path" mft; done
for path in ${IMAGES[@]+"${IMAGES[@]}"}; do run "image:$(basename "$path")" "$path" image; done

PYTHON="$(command -v python3 || command -v python)"
"$PYTHON" - "$RESULTS" "$OUTPUT/summary.md" <<'EOF'
import json, statistics, sys, collections
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
groups = collections.defaultdict(list)
for row in rows:
    groups[(row["corpus"], row["workload"])].append(row)
out = ["| corpus | workload | records | median | min | records/s | names | resolved | orphans |",
       "|---|---|---:|---:|---:|---:|---:|---:|---:|"]
for (corpus, workload), runs in groups.items():
    walls = [run["wall_s"] for run in runs]
    median = statistics.median(walls)
    last = runs[-1]
    out.append(
        f"| {corpus} | {workload} | {last['records']:,} | {median*1000:.1f} ms | {min(walls)*1000:.1f} ms "
        f"| {last['records']/median/1e6:.1f} M | {last['names']:,} | {last['resolved']:,} | {last['orphans']:,} |"
    )
open(sys.argv[2], "w").write("\n".join(out) + "\n")
print("\n".join(out))
EOF
