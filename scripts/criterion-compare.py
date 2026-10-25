#!/usr/bin/env python3
"""Summarizes Criterion comparisons as a Markdown table, with a GitHub Actions warning for each
regression past the threshold.

Two modes:

- `--runs DIR` (what scripts/criterion-ab.sh writes): runs of a base and a head revision that
  took turns, each in `DIR/<n>-<base|head>/`. A benchmark changed when every head run lies on
  the same side of every base run, by more than the threshold and by more than twice the
  spread of either side's own runs (the same code, run again: the machine's noise). The
  change is per element or byte when the benchmark declares a throughput, so that it stays
  comparable when a benchmark's batch changes size.
- otherwise, one run with `--save-baseline base` and one with `--baseline base`, from
  `target/criterion/**/change/estimates.json`: Criterion's own confidence interval, which knows
  nothing of how the machine drifted between the two.

usage: criterion-compare.py [criterion-dir | --runs DIR] [--threshold PERCENT]
                            [--fail-on-regression]
"""

import collections
import json
import os
import pathlib
import statistics
import sys


def main() -> int:
    argv = sys.argv[1:]
    threshold = float(option(argv, "--threshold") or 10.0)
    fail = "--fail-on-regression" in argv
    runs = option(argv, "--runs")
    if runs:
        regressions = compare_runs(pathlib.Path(runs), threshold)
    else:
        args = [arg for arg in argv if not arg.startswith("--") and arg != option(argv, "--threshold")]
        regressions = compare_baseline(args, threshold)
    return 1 if fail and regressions else 0


def option(argv: list, name: str):
    return argv[argv.index(name) + 1] if name in argv and argv.index(name) + 1 < len(argv) else None


def compare_runs(root: pathlib.Path, threshold: float) -> int:
    # benchmark -> side -> [time per unit, one per run]; and the time per iteration of each side
    times = collections.defaultdict(lambda: collections.defaultdict(list))
    per_iteration = collections.defaultdict(dict)
    for estimates in sorted(root.glob("*/**/new/estimates.json")):
        run = estimates.relative_to(root).parts[0]
        side = run.split("-", 1)[1]
        bench_dir = estimates.parent.parent
        bench = bench_dir.relative_to(root / run).as_posix()
        nanos = json.loads(estimates.read_text())["median"]["point_estimate"]
        times[bench][side].append(nanos / units(bench_dir / "new" / "benchmark.json"))
        per_iteration[bench][side] = nanos
    if not times:
        print("No runs found (run scripts/criterion-ab.sh first).")
        return 0
    print("| benchmark | time (head) | change | noise | |\n|---|---:|---:|---:|---|")
    regressions = 0
    for bench in sorted(times):
        base, head = times[bench].get("base", []), times[bench].get("head", [])
        if not base or not head:
            side = "head" if head else "base"
            print(f"| `{bench}` | {format_time(per_iteration[bench][side])} | | | {side} only |")
            continue
        change = (statistics.median(head) / statistics.median(base) - 1) * 100
        noise = max(spread(base), spread(head))
        apart = min(head) > max(base) or max(head) < min(base)
        mark = ""
        if apart and abs(change) > max(threshold, 2 * noise):
            if change > 0:
                mark = "regression"
                regressions += 1
                if os.environ.get("GITHUB_ACTIONS") == "true":
                    print(f"::warning title=Benchmark regression::{bench} is {change:+.1f}% slower", file=sys.stderr)
            else:
                mark = "improvement"
        elif noise > threshold:
            mark = "noisy"
        print(f"| `{bench}` | {format_time(per_iteration[bench]['head'])} | {change:+.1f}% | ±{noise:.1f}% | {mark} |")
    print(f"\n{len(times)} benchmarks, {regressions} regressions past {threshold:.0f}%. Noise is the"
          " largest spread of one side's own runs.")
    return regressions


def units(benchmark_json: pathlib.Path) -> float:
    """The elements or bytes one iteration handles, as the benchmark declares; 1 if it does not."""
    try:
        throughput = json.loads(benchmark_json.read_text()).get("throughput") or {}
    except (OSError, ValueError):
        return 1.0
    for value in throughput.values():
        if isinstance(value, (int, float)) and value > 0:
            return float(value)
    return 1.0


def spread(values: list) -> float:
    return (max(values) - min(values)) / statistics.median(values) * 100 if len(values) > 1 else 0.0


def compare_baseline(args: list, threshold: float) -> int:
    root = pathlib.Path(args[0] if args else os.environ.get("CARGO_TARGET_DIR", "target")) / "criterion"
    if not root.exists() and args:
        root = pathlib.Path(args[0])
    rows = []
    for change in sorted(root.rglob("change/estimates.json")):
        bench = change.parent.parent.relative_to(root).as_posix()
        estimate = json.loads(change.read_text())["mean"]
        point = estimate["point_estimate"] * 100
        low = estimate["confidence_interval"]["lower_bound"] * 100
        high = estimate["confidence_interval"]["upper_bound"] * 100
        new = json.loads((change.parent.parent / "new" / "estimates.json").read_text())["mean"]["point_estimate"]
        rows.append((bench, point, low, high, new))
    if not rows:
        print("No comparisons found (run with --baseline first).")
        return 0
    print("| benchmark | time (new) | change | 95% CI | |\n|---|---:|---:|---:|---|")
    regressions = 0
    for bench, point, low, high, new in rows:
        significant = low > 0 or high < 0
        mark = ""
        if significant and low > threshold:
            mark = "regression"
            regressions += 1
            if os.environ.get("GITHUB_ACTIONS") == "true":
                print(f"::warning title=Benchmark regression::{bench} is {point:+.1f}% slower", file=sys.stderr)
        elif significant and high < -threshold:
            mark = "improvement"
        print(f"| `{bench}` | {format_time(new)} | {point:+.1f}% | [{low:+.1f}%, {high:+.1f}%] | {mark} |")
    print(f"\n{len(rows)} benchmarks, {regressions} regressions past {threshold:.0f}%.")
    return regressions


def format_time(nanos: float) -> str:
    for unit, scale in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
        if nanos >= scale:
            return f"{nanos / scale:.2f} {unit}"
    return f"{nanos:.1f} ns"


if __name__ == "__main__":
    sys.exit(main())
