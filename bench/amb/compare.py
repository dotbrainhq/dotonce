"""Compare agent-memory-benchmark result files.

Usage: python3 bench/amb/compare.py [--max-context-tokens N] RESULT [RESULT ...]
Accepts .json and .json.gz. Compares only the questions present in every file,
prints overall numbers, then accuracy per question type. Exits non-zero when the
files used different answer or judge models, or when a run's average context
tokens (the runner's own count) is above --max-context-tokens.
"""

import argparse
import gzip
import json
import sys
from collections import defaultdict
from pathlib import Path


def load(path: Path) -> dict:
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt") as f:
        d = json.load(f)
    return d if isinstance(d, dict) else d[0]


def label(path: Path) -> str:
    """Run name from outputs/<dataset>/<run>/<mode>/<split>.json, else the file name."""
    parts = path.resolve().parts
    if len(parts) >= 5 and parts[-5] == "outputs":
        return parts[-3]
    name = path.name
    for ext in (".gz", ".json"):
        name = name.removesuffix(ext)
    return name


def qtype(r: dict) -> str:
    axes = r.get("category_axes") or {}
    vals = axes.get("Question Type") or [r.get("meta", {}).get("question_type", "unknown")]
    return str(vals[0])


def same_models(runs: list) -> None:
    for key in ("answer_llm", "judge_llm"):
        seen = {d.get(key) for _, _, d in runs}
        if len(seen) > 1:
            listed = ", ".join(f"{name}={d.get(key)}" for name, _, d in runs)
            sys.exit(f"{key} differs between files: {listed}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Compare agent-memory-benchmark result files.")
    ap.add_argument("results", nargs="+", type=Path)
    ap.add_argument("--max-context-tokens", type=float, default=None)
    args = ap.parse_args()

    runs = [(label(p), p, load(p)) for p in args.results]
    same_models(runs)

    for name, _, d in runs:
        print(f"{name:40} {len(d['results'])} results")
    shared = set.intersection(*({r["query_id"] for r in d["results"]} for _, _, d in runs))
    if not shared:
        sys.exit("no question is present in every file")
    print(f"comparing {len(shared)} questions present in all {len(runs)} files")
    print()

    picked = [(name, [r for r in d["results"] if r["query_id"] in shared], d) for name, _, d in runs]
    over = []
    print(f"{'run':40} {'acc':>7} {'correct':>10} {'ctx tok':>9} {'ret ms':>8}  answer / judge")
    for name, rs, d in picked:
        n = len(rs)
        ok = sum(1 for r in rs if r["correct"])
        ctx = sum(r.get("context_tokens", 0) for r in rs) / n
        ms = sum(r.get("retrieve_time_ms", 0) for r in rs) / n
        print(f"{name:40} {ok / n:7.1%} {ok:>5}/{n:<4} {ctx:9.0f} {ms:8.1f}  {d.get('answer_llm')} / {d.get('judge_llm')}")
        if args.max_context_tokens is not None and ctx > args.max_context_tokens:
            over.append(f"{name} averages {ctx:.0f} context tokens, above {args.max_context_tokens:.0f}")

    types = sorted({qtype(r) for _, rs, _ in picked for r in rs})
    width = max(20, *(len(name) + 2 for name, _, _ in picked))
    print()
    print(f"{'question type':28}" + "".join(f"{name:>{width}}" for name, _, _ in picked))
    for t in types:
        row = f"{t:28}"
        for _, rs, _ in picked:
            c = defaultdict(int)
            for r in rs:
                if qtype(r) == t:
                    c["n"] += 1
                    c["ok"] += int(r["correct"])
            row += f"{(str(c['ok']) + '/' + str(c['n'])) if c['n'] else '-':>{width}}"
        print(row)

    if over:
        sys.exit("\n".join(over))


if __name__ == "__main__":
    main()
