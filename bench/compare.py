#!/usr/bin/env python3
"""Compare benchmark runs side by side.

Usage: python3 compare.py results/run-*.jsonl
"""

import json
import sys


def load(path):
    meta, summary, turns = None, None, []
    with open(path) as f:
        for line in f:
            rec = json.loads(line)
            if rec.get("meta"):
                meta = rec
            elif rec.get("summary"):
                summary = rec
            elif "usage" in rec:
                turns.append(rec)
    return meta, summary, turns


def main():
    paths = sys.argv[1:]
    if not paths:
        sys.exit("usage: compare.py results/run-*.jsonl")

    runs = []
    for p in paths:
        meta, summary, turns = load(p)
        if meta and summary:
            runs.append((meta, summary, turns, p))

    if not runs:
        sys.exit("no complete runs found")

    hdr = (f"{'strategy':<14} {'pass':<7} {'calls':<6} {'hit%':<7} "
           f"{'in':<9} {'out':<8} {'c_read':<10} {'c_write':<9} "
           f"{'cost$':<9} {'wall_s':<7}")
    print(hdr)
    print("-" * len(hdr))
    for meta, s, turns, p in runs:
        u = s["usage"]
        print(f"{s['strategy']:<14} "
              f"{s['passed']}/{s['passed']+s['failed']:<5} "
              f"{s['api_calls']:<6} "
              f"{s['overall_hit_pct']:<7} "
              f"{u['input']:<9} {u['output']:<8} "
              f"{u['cache_read']:<10} {u['cache_write']:<9} "
              f"{s['total_cost_usd']:<9} {s['wall_s']:<7}")

    # Per-question hit% drill-down when exactly 2+ runs share question ids
    if len(runs) > 1:
        print("\nper-question hit% by strategy:")
        ids = sorted({t["q"] for _, _, turns, _ in runs for t in turns})
        names = [s["strategy"] for _, s, _, _ in runs]
        print(f"{'Q':<4}" + "".join(f"{n:<14}" for n in names))
        for qid in ids:
            row = f"{qid:<4}"
            for _, _, turns, _ in runs:
                t = next((t for t in turns if t["q"] == qid), None)
                row += f"{t['hit_pct'] if t else '-':<14}"
            print(row)


if __name__ == "__main__":
    main()
