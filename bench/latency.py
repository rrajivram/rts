"""Request-loop latency per memory mode (bench/workloads/requests.ts).

    python3 bench/latency.py            # writes bench/results/latency.json
"""
import json, os, re, subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
W = os.path.join(ROOT, "bench/workloads/requests.ts")
RC = os.environ.get("RTS", os.path.join(ROOT, "target/release/rts"))
GC = os.environ.get("RTS_GC", os.path.join(ROOT, "target/gc/release/rts"))
CONFIGS = {
    "RC + arenas": [RC],
    "RC, deferred": [RC, "--deferred-rc"],
    "RC, no arenas": [RC, "--no-arena"],
    "RC, no cycle collector": [RC, "--no-cycles"],
    "tracing GC": [GC],
    "tracing GC, tuned small": ["env", "RTS_GC_MIN_HEAP=1048576", "RTS_GC_GROW=1.5", GC],
    "node (JIT)": ["node"],
}
N = os.environ.get("REQUESTS", "100000")
RUNS = int(os.environ.get("RUNS", "3"))


def run(cmd):
    # Native code compiled up front: compilation isn't a memory-management pause.
    env = dict(os.environ, RTS_JIT_EAGER="1")
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd + [W, N], capture_output=True, text=True, check=True, env=env)
    m = re.search(r"mean ([\d.]+) p50 ([\d.]+) p99 ([\d.]+) p99.9 ([\d.]+) max ([\d.]+)", p.stderr)
    rss = int(re.search(r"(\d+)\s+maximum resident set size", p.stderr).group(1)) / 1048576
    return [float(x) for x in m.groups()] + [rss]


rows = []
for name, cmd in CONFIGS.items():
    samples = [run(cmd) for _ in range(RUNS)]
    med = [sorted(col)[len(col) // 2] for col in zip(*samples)]
    row = dict(zip(["mean_us", "p50_us", "p99_us", "p999_us", "max_us", "rss_mb"], med))
    row["config"] = name
    rows.append(row)
    print(name, row, flush=True)
out = os.path.join(ROOT, "bench/results/latency.json")
json.dump(rows, open(out, "w"), indent=1)
