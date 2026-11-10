#!/usr/bin/env python3
"""Benchmark rts against node --jitless, node (JIT) and rjs. Median of N runs, wall time."""
import os, subprocess, sys, time, statistics, shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RTS = os.path.join(ROOT, "target/release/rts")
RJS = os.path.join(ROOT, "../rjs/target/release/rjs")
N = int(os.environ.get("RUNS", "5"))

JSDIR = "/tmp/rts_bench_js"
os.makedirs(JSDIR, exist_ok=True)

def to_js(path):
    """Strip TypeScript types with Node so jitless node and rjs can run the program."""
    out = os.path.join(JSDIR, os.path.basename(path).replace(".ts", ".js"))
    subprocess.run(["node", "--no-warnings", "-e",
        "const m=require('node:module'),fs=require('fs');"
        "fs.writeFileSync(process.argv[2], m.stripTypeScriptTypes(fs.readFileSync(process.argv[1],'utf8')))",
        path, out], check=True)
    return out

def run(cmd):
    times = []
    out = None
    for _ in range(N):
        t = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, text=True)
        times.append(time.perf_counter() - t)
        if p.returncode != 0:
            return None, (p.stderr.strip().splitlines() or ["failed"])[-1][:60]
        out = p.stdout
    return statistics.median(times) * 1000, out

progs = sys.argv[1:] or sorted(f for f in os.listdir(os.path.join(ROOT, "bench")) if f.endswith(".ts"))
engines = [("rts", lambda f: [RTS, f]),
           ("node --jitless", lambda f: ["node", "--jitless", to_js(f)]),
           ("node", lambda f: ["node", to_js(f)])]
if os.path.exists(RJS):
    engines.append(("rjs", lambda f: [RJS, f]))
print(f"{'program':24}" + "".join(f"{e:>16}" for e, _ in engines))
for p in progs:
    path = os.path.join(ROOT, "bench", p)
    row = f"{p:24}"
    ref = None
    for name, cmd in engines:
        c = cmd(to_js(path) if name == "rjs" else path)
        ms, out = run(c)
        if ms is None:
            row += f"{'err':>16}"
            continue
        if ref is None:
            ref = out
        mark = "" if out == ref else "*"
        row += f"{ms:>14.1f}{mark:1}ms"[-16:].rjust(16)
    print(row, flush=True)
