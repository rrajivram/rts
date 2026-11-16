"""Per-instruction profile of the typed Are-We-Fast-Yet suite, to decide
what to optimize next (after rjs's per-instruction statistics).

    cargo build --release --features profile --target-dir target/profile
    python3 bench/profile.py              # all benchmarks; OUT=profile.json to save
    BENCH=CD,Havlak python3 bench/profile.py

For each benchmark, `rts --profile` reports every instruction kind's count,
sampled time and allocations. The suite view ranks instructions by their
average share of time across benchmarks (each benchmark weighs the same).
"""
import json, os, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RTS = os.path.join(ROOT, "target/profile/release/rts")
AWFY = os.path.join(ROOT, "bench/awfy/ts")
SIZES = {"Bounce": 1500, "CD": 250, "DeltaBlue": 12000, "Havlak": 1, "Json": 100, "List": 1500,
         "Mandelbrot": 500, "NBody": 250000, "Permute": 1000, "Queens": 1000, "Richards": 20,
         "Sieve": 3000, "Storage": 1000, "Towers": 600}


def profile(bench, n):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
        path = f.name
    env = dict(os.environ, RTS_PROFILE_JSON=path)
    subprocess.run([RTS, "--profile", "harness.ts", bench, "1", str(n)], cwd=AWFY, env=env,
                   check=True, capture_output=True)
    with open(path) as f:
        data = json.load(f)
    os.unlink(path)
    return data


def main():
    want = os.environ.get("BENCH")
    want = set(want.split(",")) if want else None
    per = {}
    for b, n in SIZES.items():
        if want and b not in want:
            continue
        d = profile(b, n)
        total = sum(r["ns"] for r in d["rows"]) or 1
        per[b] = {"wall_ns": d["wall_ns"], "rows": d["rows"], "total_ns": total}
        top = sorted(d["rows"], key=lambda r: -r["ns"])[:5]
        print(f"{b:11} " + "  ".join(f"{r['instruction']} {100 * r['ns'] / total:.0f}%" for r in top), flush=True)
    # Suite view: mean share of time per instruction.
    share, count, allocs, rc = {}, {}, {}, {}
    for b, d in per.items():
        for r in d["rows"]:
            k = r["instruction"]
            share[k] = share.get(k, 0) + r["ns"] / d["total_ns"] / len(per)
            count[k] = count.get(k, 0) + r["count"]
            allocs[k] = allocs.get(k, 0) + r["allocs"]
            rc[k] = rc.get(k, 0) + r.get("rc", 0)
    print()
    print(f"{'instruction':<24} {'mean time%':>10} {'count (all)':>14} {'allocs (all)':>13} {'rc ops/op':>9}")
    ranked = sorted(share.items(), key=lambda kv: -kv[1])
    for k, v in ranked[:30]:
        per = rc[k] / count[k] if count[k] else 0
        print(f"{k:<24} {100 * v:>9.1f}% {count[k]:>14,} {allocs[k]:>13,} {per:>9.2f}")
    print(f"\nreference-count updates per instruction executed: {sum(rc.values()) / max(1, sum(count.values())):.3f}")
    out = os.environ.get("OUT")
    if out:
        with open(out, "w") as f:
            json.dump({"benchmarks": per, "suite": [{"instruction": k, "share": v, "count": count[k], "allocs": allocs[k], "rc": rc[k]} for k, v in ranked]}, f, indent=1)


if __name__ == "__main__":
    main()
