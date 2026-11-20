"""Heap census across the thesis benchmarks: evidence for the memory-model
decision (see src/runtime/census.rs and docs/memory-model.md).

    python3 bench/census.py              # writes bench/results/census.json
    BENCH=DeltaBlue,Havlak python3 bench/census.py

Per program:
- how much of what it allocates is of cyclic kinds (kinds in a cycle of the
  kind graph: what a type-graph analysis finds), and which kinds those are;
- the cycle collector: boxes traced, and the share of that tracing that
  starts from candidates of cyclic kinds (what static acyclicity alone
  could not remove);
- the region design simulated: memory at snapshots for this build versus
  the design (unreachable regions freed at once, floating garbage held
  in the others), and the largest region (the bound on any region-local
  work).
"""
import json, os, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RTS = os.environ.get("RTS", os.path.join(ROOT, "target/release/rts"))
AWFY = os.path.join(ROOT, "bench/awfy/ts")
PROGRAMS = {
    "Havlak": (AWFY, ["harness.ts", "Havlak", "5", "1500"]),
    "CD": (AWFY, ["harness.ts", "CD", "5", "250"]),
    "DeltaBlue": (AWFY, ["harness.ts", "DeltaBlue", "5", "12000"]),
    "Json": (AWFY, ["harness.ts", "Json", "5", "100"]),
    "Storage": (AWFY, ["harness.ts", "Storage", "5", "1000"]),
    "Richards": (AWFY, ["harness.ts", "Richards", "5", "100"]),
    "requests": (ROOT, [os.path.join(ROOT, "bench/workloads/requests.ts"), "300000"]),
    # The hard case: a long-lived cyclic structure losing cyclic pieces.
    "churn": (ROOT, [os.path.join(ROOT, "bench/workloads/churn.ts"), "20000"]),
}
MB = 1048576


def summarize(d):
    kinds = d["kinds"]
    tot = sum(k["bytes"] for k in kinds) or 1
    cyc = [k for k in kinds if k["cyclic"]]
    cands = sum(k["candidates"] for k in kinds) or 1
    cols = d["collections"]  # at_alloc, candidates, traced, garbage, ns, cyclic_candidates, traced_cyclic_only
    snaps = d["snapshots"]  # at_alloc, live, reachable, then per rule: design, floating, largest, largest_members, regions
    traced = sum(c[2] for c in cols) or 1
    n = len(snaps) or 1
    mean = lambda i: round(sum(s[i] for s in snaps) / n / MB, 2)
    peak = lambda i: round(max((s[i] for s in snaps), default=0) / MB, 2)
    r = {
        "cyclic_kinds": [k["kind"] for k in cyc],
        "cyclic_alloc_pct": round(100 * sum(k["bytes"] for k in cyc) / tot, 1),
        "allocated_mb": round(tot / MB, 1),
        "cyclic_candidates_pct": round(100 * sum(k["candidates"] for k in cyc) / cands, 1),
        "collections": len(cols),
        "cc_traced": sum(c[2] for c in cols),
        "cc_garbage": sum(c[3] for c in cols),
        "cc_traced_cyclic_pct": round(100 * sum(c[6] for c in cols) / traced, 1),
        "garbage_kinds": {k["kind"]: k["garbage"] for k in kinds if k["garbage"]},
        "rc_mean_mb": mean(1), "rc_max_mb": peak(1),
        "exact_mean_mb": mean(2), "exact_max_mb": peak(2),
        "snapshots": len(snaps),
        "unseen_cyclic_mb": round(d["unseen_cyclic_bytes"] / MB, 2),
    }
    for i, rule in enumerate(d["rules"]):
        o = 3 + 5 * i
        r[rule] = {"mean_mb": mean(o), "max_mb": peak(o), "floating_max_mb": peak(o + 1),
                   "largest_region_mb": peak(o + 2), "region_frees": d["region_frees"][i]}
    return r


def main():
    want = os.environ.get("BENCH")
    want = set(want.split(",")) if want else None
    rows = []
    for name, (cwd, args) in PROGRAMS.items():
        if want and name not in want:
            continue
        with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
            path = f.name
        p = subprocess.run([RTS] + args, cwd=cwd, capture_output=True, text=True, env=dict(os.environ, RTS_CENSUS=path))
        if p.returncode != 0:
            print(f"{name}: FAILED {p.stderr[-500:]}", file=sys.stderr)
            continue
        r = summarize(json.load(open(path)))
        os.unlink(path)
        r["bench"] = name
        rows.append(r)
        print(f"{name:10} cyclic {r['cyclic_alloc_pct']:5.1f}% of {r['allocated_mb']:7.1f} MB  "
              f"cc traced {r['cc_traced']:9} ({r['cc_traced_cyclic_pct']:5.1f}% from cyclic kinds)  "
              f"MB mean/max: rc {r['rc_mean_mb']:.2f}/{r['rc_max_mb']:.2f}  exact {r['exact_mean_mb']:.2f}/{r['exact_max_mb']:.2f}  "
              + "  ".join(f"{k} {r[k]['mean_mb']:.2f}/{r[k]['max_mb']:.2f} (largest {r[k]['largest_region_mb']:.2f})" for k in ("merge", "owner"))
              + f"  [{' '.join(r['cyclic_kinds'])}]", flush=True)
    out = os.environ.get("OUT", os.path.join(ROOT, "bench/results/census.json"))
    json.dump(rows, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
