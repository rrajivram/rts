"""Memory over time and collector pauses per memory strategy: the thesis
measurements (memory that stays close to what the program uses, and no
long pauses).

    python3 bench/memory.py              # writes bench/results/memory.json
    BENCH=Havlak,CD CONFIGS=rc,gc python3 bench/memory.py

Each run sets RTS_MEM_TRACE (see src/runtime/memtrace.rs): live heap bytes
sampled every 250 us, and every pause (cycle collection, tracing GC,
deferred-count settle) with its time. Native code is compiled up front
(RTS_JIT_EAGER=1) so compilation doesn't show as a pause.

Per run: the program's memory (live bytes above what was live when it
started), as peak, time-weighted mean and how far the peak sits above the
mean; pauses as count, percentiles, longest and total; and a timeline
reduced to at most 300 points (each the bucket's maximum, so peaks stay).
"""
import json, os, statistics, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RC = os.environ.get("RTS", os.path.join(ROOT, "target/release/rts"))
GC = os.environ.get("RTS_GC", os.path.join(ROOT, "target/gc/release/rts"))
# Ownership reference counting (docs/memory-model.md): `--features owners`.
OWN = os.environ.get("RTS_OWNERS", os.path.join(ROOT, "target/owners/release/rts"))
CONFIGS = {
    "rc": [RC],
    "rc-owners": [OWN],
    "rc-deferred": [RC, "--deferred-rc"],
    "gc": [GC],
    "gc-1.5": ["env", "RTS_GC_GROW=1.5", GC],
    # Tuned for small memory: collect from 1 MB, grow 1.5x.
    "gc-small": ["env", "RTS_GC_MIN_HEAP=1048576", "RTS_GC_GROW=1.5", GC],
}
AWFY = os.path.join(ROOT, "bench/awfy/ts")
# Allocation-heavy benchmarks, standard sizes, 5 iterations (steady state).
PROGRAMS = {
    "Havlak": (AWFY, ["harness.ts", "Havlak", "5", "1500"]),
    "CD": (AWFY, ["harness.ts", "CD", "5", "250"]),
    "DeltaBlue": (AWFY, ["harness.ts", "DeltaBlue", "5", "12000"]),
    "Json": (AWFY, ["harness.ts", "Json", "5", "100"]),
    "Storage": (AWFY, ["harness.ts", "Storage", "5", "1000"]),
    "Richards": (AWFY, ["harness.ts", "Richards", "5", "100"]),
    "requests": (ROOT, [os.path.join(ROOT, "bench/workloads/requests.ts"), "300000"]),
    "churn": (ROOT, [os.path.join(ROOT, "bench/workloads/churn.ts"), "20000"]),
}
POINTS = 300


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def run(cmd, cwd):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
        path = f.name
    env = dict(os.environ, RTS_JIT_EAGER="1", RTS_MEM_TRACE=path)
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=env)
    if p.returncode != 0:
        raise RuntimeError(p.stderr[-2000:])
    d = json.load(open(path))
    os.unlink(path)
    base = d["start_bytes"]
    s = [(t, max(0, b - base)) for t, b in d["samples"] if t > 0]
    # Skip start-up (before the program runs, live bytes are below base).
    while len(s) > 1 and s[0][1] == 0:
        s.pop(0)
    mb = [b / 1048576 for _, b in s]
    # Time-weighted mean.
    tot = sum((s[i + 1][0] - s[i][0]) * s[i][1] for i in range(len(s) - 1))
    span = (s[-1][0] - s[0][0]) or 1
    mean = tot / span / 1048576
    pauses = [p[1] / 1e6 for p in d["pauses"]]
    kinds = {}
    for p in d["pauses"]:
        k = kinds.setdefault(p[2], [])
        k.append(p[1] / 1e6)
    by_kind = {k: {"n": len(v), "max_ms": round(max(v), 3), "total_ms": round(sum(v), 1)} for k, v in kinds.items()}
    k = max(1, len(s) // POINTS)
    t0 = s[0][0]
    timeline = [[round((s[i][0] - t0) / 1000, 2), round(max(mb[i:i + k]), 3)] for i in range(0, len(s), k)]
    return {
        "peak_mb": round(max(mb), 3), "mean_mb": round(mean, 3), "p50_mb": round(statistics.median(mb), 3),
        "peak_over_mean": round(max(mb) / mean, 2) if mean > 0 else None,
        "pauses": len(pauses), "pause_p50_ms": round(pct(pauses, 50), 3), "pause_p99_ms": round(pct(pauses, 99), 3),
        "pause_max_ms": round(max(pauses, default=0), 3), "pause_total_ms": round(sum(pauses), 1),
        "duration_s": round(span / 1e6, 3), "by_kind": by_kind, "timeline": timeline,
    }


def main():
    want = os.environ.get("BENCH")
    want = set(want.split(",")) if want else None
    configs = os.environ.get("CONFIGS", ",".join(CONFIGS)).split(",")
    rows = []
    for name, (cwd, args) in PROGRAMS.items():
        if want and name not in want:
            continue
        for c in configs:
            try:
                r = run(CONFIGS[c] + args, cwd)
            except RuntimeError as e:
                print(f"{name:10} {c:12} FAILED: {str(e).splitlines()[-1] if str(e) else ''}", file=sys.stderr)
                continue
            r.update(bench=name, config=c)
            rows.append(r)
            print(f"{name:10} {c:12} peak {r['peak_mb']:8.2f} MB  mean {r['mean_mb']:8.2f}  peak/mean {r['peak_over_mean']}  "
                  f"pauses {r['pauses']:5} p99 {r['pause_p99_ms']:7.3f} max {r['pause_max_ms']:8.3f} ms  "
                  + " ".join(f"{k}:{v['n']}/{v['max_ms']}" for k, v in r["by_kind"].items()), flush=True)
    out = os.environ.get("OUT", os.path.join(ROOT, "bench/results/memory.json"))
    json.dump(rows, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
