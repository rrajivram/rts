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
    gc         tracing mark-sweep GC (heap grows 2x the survivors between collections)
    gc-1.25, gc-1.5, gc-3
               the same GC with another growth factor (RTS_GC_GROW): smaller
               factors collect more often in less memory (a time-space curve)
    node       Node.js (V8, JIT)
    jitless    node --jitless (V8's interpreter)
"""
import os, re, shutil, statistics, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RC = os.environ.get("RTS", os.path.join(ROOT, "target/release/rts"))
GC = os.environ.get("RTS_GC", os.path.join(ROOT, "target/gc/release/rts"))
BASE = os.environ.get("BASE")  # optional: another rts binary, as config "base"
N = int(os.environ.get("RUNS", "5"))

CONFIGS = {
    "rc": [RC, "--mem-stats"],
    "rc-noarena-nocc": [RC, "--mem-stats", "--no-arena", "--no-cycles"],
    "rc-noarena": [RC, "--mem-stats", "--no-arena"],
    "rc-nocc": [RC, "--mem-stats", "--no-cycles"],
    "gc": [GC, "--mem-stats"],
    "gc-1.25": ["env", "RTS_GC_GROW=1.25", GC, "--mem-stats"],
    "gc-1.5": ["env", "RTS_GC_GROW=1.5", GC, "--mem-stats"],
    "gc-3": ["env", "RTS_GC_GROW=3", GC, "--mem-stats"],
    "node": ["node"],
    "jitless": ["node", "--jitless"],
}
if BASE:
    CONFIGS["base"] = [BASE]

SUITE = os.environ.get("SUITE", "ts")  # ts: typed ports; js: upstream JavaScript
AWFY = os.path.join(ROOT, "bench/awfy", SUITE)
HARNESS = "harness.ts" if SUITE == "ts" else "harness.js"
# Inner iterations: the suite's standard sizes, except Richards and Havlak,
# which are cut so an interpreter finishes in a few seconds (Havlak verifies
# only at 1, 15, 150, ...).
SIZES = {"Bounce": 1500, "CD": 250, "DeltaBlue": 12000, "Havlak": 1, "Json": 100, "List": 1500,
         "Mandelbrot": 500, "NBody": 250000, "Permute": 1000, "Queens": 1000, "Richards": 20,
         "Sieve": 3000, "Storage": 1000, "Towers": 600}


def node_copy():
    """Node can't strip TypeScript without WebAssembly (so not with --jitless):
    give it a type-stripped copy of the typed suite, imports renamed to .js."""
    if SUITE != "ts":
        return AWFY, "harness.js"
    out = tempfile.mkdtemp(prefix="rts-awfy-")
    strip = ("const m=require('node:module'),fs=require('fs'),path=require('path');"
             "const [src,dst]=process.argv.slice(1);"
             "for(const f of fs.readdirSync(src)){if(!f.endsWith('.ts'))continue;"
             "let js=m.stripTypeScriptTypes(fs.readFileSync(path.join(src,f),'utf8'));"
             "js=js.replace(/from '(\\.\\/[^']+)\\.ts'/g,\"from '$1.js'\");"
             "fs.writeFileSync(path.join(dst,f.replace(/\\.ts$/,'.js')),js);}"
             "fs.writeFileSync(path.join(dst,'package.json'),'{\"type\":\"module\"}');")
    subprocess.run(["node", "-e", strip, AWFY, out], check=True)
    return out, "harness.js"


def programs():
    want = os.environ.get("BENCH")
    want = set(want.split(",")) if want else None
    for b, n in SIZES.items():
        if want is None or b in want:
            yield b, [b, "1", str(n)]


def run(cmd, cwd):
    # rts compiles native code lazily, during the first iteration; compile it
    # all up front so the harness's time is the benchmark's steady state.
    env = dict(os.environ, RTS_JIT_EAGER="1")
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd, cwd=cwd, capture_output=True, text=True, env=env)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{p.stderr[-2000:]}")
    err = p.stderr
    rss = int(re.search(r"(\d+)\s+maximum resident set size", err).group(1))
    wall = float(re.search(r"([\d.]+) real", err).group(1))
    m = re.search(r"runtime: (\d+)us", p.stdout)
    if m:
        # The harness's own measurement: the benchmark only, without
        # start-up and compilation.
        wall = int(m.group(1)) / 1e6
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
    node_dir, node_harness = node_copy() if any(c in ("node", "jitless") for c in configs) else (AWFY, HARNESS)
    for name, bargs in programs():
        for c in configs:
            is_node = c in ("node", "jitless")
            cwd = node_dir if is_node else AWFY
            args = [node_harness if is_node else HARNESS] + bargs
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
