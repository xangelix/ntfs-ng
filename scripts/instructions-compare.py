#!/usr/bin/env python3
"""Summarizes gungraun's instruction counts (benches/instructions.rs) as a Markdown table: each
benchmark's instructions and estimated cycles, and their change against a saved baseline, with
a GitHub Actions warning for each benchmark whose instructions grew past the threshold. Counts
are exact: parsers a change did not touch count the same.

Run the base revision with `--save-baseline=base`, then this one with `--baseline=base
--output-format=json` into a file, and pass that file:

usage: instructions-compare.py SUMMARY.jsonl [--threshold PERCENT] [--fail-on-regression]
"""

import json
import os
import sys


def main() -> int:
    argv = sys.argv[1:]
    paths = [arg for arg in argv if not arg.startswith("--") and arg != option(argv, "--threshold")]
    threshold = float(option(argv, "--threshold") or 1.0)
    rows = []
    for path in paths:
        with open(path, encoding="utf-8") as lines:
            for line in lines:
                line = line.strip()
                if line.startswith("{"):
                    rows.append(row(json.loads(line)))
    if not rows:
        print("No instruction counts found.")
        return 0
    compared = any(old is not None for _, _, old, _, _ in rows)
    print("| benchmark | instructions | change | estimated cycles | change | |")
    print("|---|---:|---:|---:|---:|---|")
    regressions = changed = 0
    for bench, new, old, cycles, old_cycles in sorted(rows):
        change = percent(new, old)
        mark = ""
        if change is not None and change != 0:
            changed += 1
            if change > threshold:
                mark = "regression"
                regressions += 1
                if os.environ.get("GITHUB_ACTIONS") == "true":
                    print(f"::warning title=Instruction count regression::{bench}: {change:+.2f}% instructions", file=sys.stderr)
            elif change < -threshold:
                mark = "improvement"
        print(f"| `{bench}` | {new:,} | {shown(change)} | {cycles:,} | {shown(percent(cycles, old_cycles))} | {mark} |")
    if compared:
        print(f"\n{len(rows)} benchmarks: {changed} changed, {regressions} by more than {threshold:g}% more"
              " instructions.")
    else:
        print(f"\n{len(rows)} benchmarks; no baseline to compare with.")
    return 1 if "--fail-on-regression" in argv and regressions else 0


def option(argv: list, name: str):
    return argv[argv.index(name) + 1] if name in argv and argv.index(name) + 1 < len(argv) else None


def row(summary: dict):
    bench = f"{summary['group']}::{summary['function_name']} {summary['id']}"
    metrics = summary["profiles"][0]["data"]["parts"][0]["metrics"]

    def values(name):
        both = metrics.get(name, {}).get("values", {})
        return both.get("new"), both.get("old")

    new, old = values("Ir")
    cycles, old_cycles = values("EstimatedCycles")
    return bench, new, old, cycles, old_cycles


def percent(new, old):
    if new is None or old is None or old == 0:
        return None
    return (new / old - 1) * 100


def shown(change):
    if change is None:
        return ""
    return "0" if change == 0 else f"{change:+.2f}%"


if __name__ == "__main__":
    sys.exit(main())
