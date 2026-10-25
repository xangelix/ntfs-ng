#!/usr/bin/env python3
"""Summarizes Criterion's comparison against a saved baseline.

Run the benchmarks with `--save-baseline base` on the base revision, then with
`--baseline base` on the head revision: Criterion writes each benchmark's change under
`target/criterion/**/change/estimates.json`. This prints a Markdown table of every change and,
on GitHub Actions, a warning annotation for each regression past the threshold.

usage: criterion-compare.py [criterion-dir] [--threshold PERCENT] [--fail-on-regression]
"""

import json
import os
import pathlib
import sys


def main() -> int:
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    root = pathlib.Path(args[0] if args else os.environ.get("CARGO_TARGET_DIR", "target")) / "criterion"
    if not root.exists() and args:
        root = pathlib.Path(args[0])
    threshold = 10.0
    if "--threshold" in sys.argv:
        threshold = float(sys.argv[sys.argv.index("--threshold") + 1])
    fail = "--fail-on-regression" in sys.argv
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
    print(f"| benchmark | time (new) | change | 95% CI | |\n|---|---:|---:|---:|---|")
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
    return 1 if fail and regressions else 0


def format_time(nanos: float) -> str:
    for unit, scale in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
        if nanos >= scale:
            return f"{nanos / scale:.2f} {unit}"
    return f"{nanos:.1f} ns"


if __name__ == "__main__":
    sys.exit(main())
