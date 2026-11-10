#!/usr/bin/env python3
"""What do static types buy? Same engine, same source, two compilations:

  typed     rts uses the program's types (unboxed numbers, slot fields, direct calls)
  untyped   rts --no-types: every variable/param/return/field/`this` is `any`, so
            every operation takes the dynamic, tag-checking path

node --jitless (V8's Ignition interpreter) is shown as a reference point for
how fast a production dynamic interpreter is, so the untyped column can be
judged as a realistic baseline rather than a strawman.

Median wall time of RUNS runs (default 7). Outputs are checked to be identical.
"""
import os, subprocess, sys, time, statistics

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RTS = os.path.join(ROOT, "target/release/rts")
N = int(os.environ.get("RUNS", "7"))
JSDIR = "/tmp/rts_bench_js"
os.makedirs(JSDIR, exist_ok=True)


def to_js(path):
    out = os.path.join(JSDIR, os.path.basename(path).replace(".ts", ".js"))
    subprocess.run(["node", "--no-warnings", "-e",
        "const m=require('node:module'),fs=require('fs');"
        "fs.writeFileSync(process.argv[2], m.stripTypeScriptTypes(fs.readFileSync(process.argv[1],'utf8')))",
        path, out], check=True)
    return out


def run(cmd):
    times, out = [], None
    for _ in range(N):
        t = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, text=True)
        times.append(time.perf_counter() - t)
        if p.returncode != 0:
            sys.exit(f"failed: {' '.join(cmd)}\n{p.stderr}")
        out = p.stdout
    return statistics.median(times) * 1000, out


def static_count(args, path):
    """Instructions in the compiled program, and how many are dynamic/boxing ops."""
    d = subprocess.run([RTS, "--dump", *args, path], capture_output=True, text=True).stdout
    ops = [l.split()[1] for l in d.splitlines() if l.startswith("  ") and len(l.split()) > 1]
    dyn = sum(o.startswith(("Dyn", "Box", "Unbox", "GetProp", "SetProp", "GetDyn", "SetDyn", "ToNum")) for o in ops)
    return len(ops), dyn


progs = sys.argv[1:] or sorted(f for f in os.listdir(os.path.join(ROOT, "bench")) if f.endswith(".ts"))
print(f"{'program':20}{'typed':>10}{'untyped':>10}{'speedup':>9}{'node --jitless':>16}   dynamic ops typed→untyped")
for p in progs:
    path = os.path.join(ROOT, "bench", p)
    t, out_t = run([RTS, path])
    u, out_u = run([RTS, "--no-types", path])
    j, out_j = run(["node", "--jitless", to_js(path)])
    assert out_t == out_u == out_j, f"{p}: outputs differ"
    nt, dt = static_count([], path)
    nu, du = static_count(["--no-types"], path)
    print(f"{p:20}{t:>8.1f}ms{u:>8.1f}ms{u / t:>8.1f}×{j:>14.1f}ms   {dt:>4}/{nt:<5}→ {du:>4}/{nu}", flush=True)
