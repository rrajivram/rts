"""Compare memory strategies on the same programs: wall time, peak RSS and
(for rts) peak heap, collector pauses and scope-arena use.

    python3 bench/modes.py                    # all configurations, AWFY + micro benchmarks
    CONFIGS=rc,gc RUNS=3 python3 bench/modes.py
    BENCH=Richards,CD python3 bench/modes.py

Configurations (rts builds: target/release = reference counting,
target/gc/release = `cargo build --release --features gc --target-dir target/gc`):

    rc         reference counting + cycle collector + scope arenas (the default)
    rc-noarena reference counting + cycle collector, no scope arenas
    rc-nocc    reference counting only (cycles leak)
    gc         tracing mark-sweep GC
    node       Node.js (V8, JIT)
    jitless    node --jitless (V8's interpreter)
"""
import os, re, statistics, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RC = os.path.join(ROOT, "target/release/rts")
GC = os.path.join(ROOT, "target/gc/release/rts")
BASE = os.environ.get("BASE")  # optional: another rts binary, as config "base"
N = int(os.environ.get("RUNS", "5"))

CONFIGS = {
    "rc": [RC, "--mem-stats"],
    "rc-noarena-nocc": [RC, "--mem-stats", "--no-arena", "--no-cycles"],
    "rc-noarena": [RC, "--mem-stats", "--no-arena"],
    "rc-nocc": [RC, "--mem-stats", "--no-cycles"],
    "gc": [GC, "--mem-stats"],
    "node": ["node"],
    "jitless": ["node", "--jitless"],
}
if BASE:
    CONFIGS["base"] = [BASE]

SUITE = os.environ.get("SUITE", "ts")  # ts: typed ports; js: upstream JavaScript
AWFY = os.path.join(ROOT, "bench/awfy", SUITE)
HARNESS = "harness.ts" if SUITE == "ts" else "harness.js"
# AWFY at a fixed size that runs ~0.1-2 s in rts (inner iterations).
SIZES = {"Bounce": 100, "CD": 100, "DeltaBlue": 2000, "Havlak": 1, "Json": 10, "List": 100,
         "Mandelbrot": 500, "NBody": 250000, "Permute": 100, "Queens": 100, "Richards": 5,
         "Sieve": 300, "Storage": 50, "Towers": 50}


def programs():
    want = os.environ.get("BENCH")
    want = set(want.split(",")) if want else None
    for b, n in SIZES.items():
        if want is None or b in want:
            yield b, [HARNESS, b, "1", str(n)], AWFY


def run(cmd, cwd):
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd, cwd=cwd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{p.stderr[-2000:]}")
    err = p.stderr
    rss = int(re.search(r"(\d+)\s+maximum resident set size", err).group(1))
    wall = float(re.search(r"([\d.]+) real", err).group(1))
    stats = {}
    for key, pat in [("heap", r"peak heap\s+([\d.]+) MB"), ("pauses", r"pauses\s+(\d+)"),
                     ("pause_max", r"max ([\d.]+) ms\)"), ("pause_total", r"total ([\d.]+) ms"),
                     ("arena", r"arena boxes\s+(\d+)"), ("collected", r"collected\s+(\d+)"),
                     ("objects", r"objects\s+(\d+) allocated"), ("escaped", r"(\d+) outlived")]:
        m = re.search(pat, err)
        if m:
            stats[key] = float(m.group(1))
    return wall, rss / (1024 * 1024), stats


def main():
    import json
    configs = os.environ.get("CONFIGS", ",".join(CONFIGS)).split(",")
    rows = []
    for name, args, cwd in programs():
        for c in configs:
            walls, rsss, last = [], [], {}
            try:
                for _ in range(N):
                    w, r, st = run(CONFIGS[c] + args, cwd)
                    walls.append(w)
                    rsss.append(r)
                    last = st
            except RuntimeError as e:
                print(f"{name:12} {c:11} FAILED: {str(e).splitlines()[0]}", file=sys.stderr)
                continue
            row = (name, c, statistics.median(walls), max(rsss), last)
            rows.append(row)
            extra = " ".join(f"{k}={v:g}" for k, v in last.items())
            print(f"{name:12} {c:11} {row[2]:7.3f}s  rss {row[3]:7.1f} MB  {extra}", flush=True)
    out = os.environ.get("OUT")
    if out:
        with open(out, "w") as f:
            json.dump([{"bench": r[0], "config": r[1], "wall": r[2], "rss": r[3], **r[4]} for r in rows], f, indent=1)
    return rows


if __name__ == "__main__":
    main()
