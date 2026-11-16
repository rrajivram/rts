"""Turn raw results into the bench page's data files (bench/results/page/*.json).

    python3 bench/results/build.py [--step "what changed"]

Inputs: modes_ts.json (bench/modes.py), profile.json (bench/profile.py),
latency.json (bench/latency.py). With --step, appends the current suite
geometric means to history.json under the current commit.
"""
import json, math, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "page")
os.makedirs(OUT, exist_ok=True)
LABEL = {"rc": "RC + arenas", "rc-noarena": "RC, no arenas", "rc-nocc": "RC, no cycle collector",
         "gc": "tracing GC", "node": "node (JIT)", "jitless": "node --jitless"}


def load(name):
    return json.load(open(os.path.join(HERE, name)))


modes = load("modes_ts.json")
rows = []
for r in modes:
    rows.append({
        "benchmark": r["bench"], "config": LABEL.get(r["config"], r["config"]),
        "time_s": round(r["wall"], 4), "rss_mb": round(r["rss"], 1),
        "peak_heap_mb": r.get("heap"), "max_pause_ms": r.get("pause_max"),
        "objects": int(r["objects"]) if "objects" in r else None,
        "arena_objects": int(r["arena"]) if "arena" in r else None,
    })
json.dump(rows, open(os.path.join(OUT, "modes.json"), "w"), indent=1)

benches = sorted({r["bench"] for r in modes})
by = {(r["bench"], r["config"]): r for r in modes}
geo = lambda c: math.exp(sum(math.log(by[(b, c)]["wall"]) for b in benches) / len(benches))

prof = load("profile.json")
prow = [{"instruction": s["instruction"], "time_share_pct": round(100 * s["share"], 2), "count": s["count"],
         "allocs": s["allocs"], "rc_ops_per_op": round(s["rc"] / s["count"], 3) if s["count"] else 0}
        for s in prof["suite"] if s["instruction"] != "<outside>"]
json.dump(prow, open(os.path.join(OUT, "profile.json"), "w"), indent=1)

if os.path.exists(os.path.join(HERE, "latency.json")):
    json.dump(load("latency.json"), open(os.path.join(OUT, "latency.json"), "w"), indent=1)

hist = load("history.json")
if "--step" in sys.argv:
    step = sys.argv[sys.argv.index("--step") + 1]
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    date = subprocess.run(["git", "log", "-1", "--format=%cs"], capture_output=True, text=True).stdout.strip()
    hist = [h for h in hist if h["step"] != step]
    hist.append({"commit": commit, "date": date, "step": step, "rc_geomean_s": round(geo("rc"), 4),
                 "gc_geomean_s": round(geo("gc"), 4), "jitless_geomean_s": round(geo("jitless"), 4)})
    json.dump(hist, open(os.path.join(HERE, "history.json"), "w"), indent=1)
json.dump(hist, open(os.path.join(OUT, "history.json"), "w"), indent=1)
print({c: round(geo(c), 4) for c in LABEL})
