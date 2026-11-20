"""Turn raw results into a self-contained local report (bench/results/report.html).

    python3 bench/results/build.py [--step "what changed"]

Inputs: modes_ts.json (bench/modes.py), profile.json (bench/profile.py),
latency.json (bench/latency.py). With --step, appends the current suite
geometric means to history.json under the current commit. The report has
its data built in and loads nothing: open it straight from disk.
"""
import html, json, math, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
LABEL = {"rc": "RC + arenas", "rc-owners": "RC, ownership", "rc-deferred": "RC, deferred", "rc-noarena": "RC, no arenas", "rc-nocc": "RC, no cycle collector",
         "gc": "tracing GC", "gc-1.25": "GC, grow 1.25×", "gc-1.5": "GC, grow 1.5×", "gc-3": "GC, grow 3×",
         "node": "node (JIT)", "jitless": "node --jitless"}
GROW = [("gc-1.25", 1.25), ("gc-1.5", 1.5), ("gc", 2.0), ("gc-3", 3.0)]


def load(name):
    p = os.path.join(HERE, name)
    return json.load(open(p)) if os.path.exists(p) else None


modes = load("modes_ts.json")
benches = sorted({r["bench"] for r in modes})
configs = [c for c in LABEL if any(r["config"] == c for r in modes)]
by = {(r["bench"], r["config"]): r for r in modes}


def geo(c, key="wall"):
    xs = [by[(b, c)][key] for b in benches if (b, c) in by and by[(b, c)].get(key)]
    return math.exp(sum(map(math.log, xs)) / len(xs)) if xs else None


def geo_ratio(a, b, key="wall"):
    xs = [by[(x, a)][key] / by[(x, b)][key] for x in benches
          if (x, a) in by and (x, b) in by and by[(x, a)].get(key) and by[(x, b)].get(key)]
    return math.exp(sum(map(math.log, xs)) / len(xs)) if xs else None


hist = load("history.json") or []
if "--step" in sys.argv:
    step = sys.argv[sys.argv.index("--step") + 1]
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    date = subprocess.run(["git", "log", "-1", "--format=%cs"], capture_output=True, text=True).stdout.strip()
    hist = [h for h in hist if h["step"] != step]
    hist.append({"commit": commit, "date": date, "step": step, "rc_geomean_s": round(geo("rc"), 4),
                 "gc_geomean_s": round(geo("gc"), 4), "jitless_geomean_s": round(geo("jitless"), 4)})
    json.dump(hist, open(os.path.join(HERE, "history.json"), "w"), indent=1)

esc = html.escape


def fmt(x, d=3):
    return "—" if x is None else f"{x:.{d}f}"


def table(head, rows, cls=""):
    h = "".join(f"<th>{esc(c)}</th>" for c in head)
    b = "".join("<tr>" + "".join(f"<td>{c}</td>" for c in r) + "</tr>" for r in rows)
    return f'<div class="wrap"><table class="{cls}"><thead><tr>{h}</tr></thead><tbody>{b}</tbody></table></div>'


def ratio_bars(items, title):
    """Horizontal bars of RC/GC ratios on a log scale centred at 1."""
    W, row, left, mid = 640, 22, 110, 375
    lo, hi = math.log(0.25), math.log(4)
    x = lambda v: mid + (math.log(max(0.25, min(4, v))) / (hi if v >= 1 else -lo)) * (W - 20 - mid if v >= 1 else mid - left)
    H = row * len(items) + 30
    out = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">']
    for t in (0.25, 0.5, 1, 2, 4):
        out.append(f'<line x1="{x(t):.1f}" x2="{x(t):.1f}" y1="0" y2="{H - 20}" class="grid{" one" if t == 1 else ""}"/>'
                   f'<text x="{x(t):.1f}" y="{H - 5}" text-anchor="middle" class="tick">{t:g}×</text>')
    for i, (name, v) in enumerate(items):
        y = i * row + 4
        if v is None:
            continue
        x0, x1 = sorted((x(1), x(v)))
        c = "win" if v < 1 else "lose"
        out.append(f'<text x="{left - 8}" y="{y + 13}" text-anchor="end" class="lbl">{esc(name)}</text>'
                   f'<rect x="{x0:.1f}" y="{y}" width="{max(1, x1 - x0):.1f}" height="{row - 6}" class="{c}"><title>{esc(name)}: {v:.2f}×</title></rect>'
                   f'<text x="{(x1 + 4) if v >= 1 else (x0 - 4):.1f}" y="{y + 13}" text-anchor="{"start" if v >= 1 else "end"}" class="val">{v:.2f}×</text>')
    out.append("</svg>")
    return "".join(out)


def tradeoff():
    """The GC at each growth factor against RC: suite geomean time and
    peak-RSS ratios, as a scatter (RC at 1×, 1×)."""
    pts = [(f, geo_ratio(c, "rc"), geo_ratio(c, "rc", "rss"), geo_ratio(c, "rc", "heap")) for c, f in GROW if any(r["config"] == c for r in modes)]
    if len(pts) < 2:
        return ""
    W, H, L, B = 640, 300, 60, 40
    d = (geo_ratio("rc-deferred", "rc"), geo_ratio("rc-deferred", "rc", "rss")) if any(r["config"] == "rc-deferred" for r in modes) else None
    xs = [p[2] for p in pts] + [1.0] + ([d[1]] if d else [])
    ys = [p[1] for p in pts] + [1.0] + ([d[0]] if d else [])
    x0, x1 = min(xs) * 0.9, max(xs) * 1.1
    y0, y1 = min(ys) * 0.9, max(ys) * 1.1
    X = lambda v: L + (v - x0) / (x1 - x0) * (W - L - 20)
    Y = lambda v: H - B - (v - y0) / (y1 - y0) * (H - B - 20)
    o = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="GC time-space tradeoff">']
    o.append(f'<line x1="{X(1):.1f}" x2="{X(1):.1f}" y1="10" y2="{H - B}" class="one"/><line x1="{L}" x2="{W - 10}" y1="{Y(1):.1f}" y2="{Y(1):.1f}" class="one"/>')
    o.append(f'<text x="{(L + W) / 2}" y="{H - 6}" text-anchor="middle" class="tick">peak RSS ÷ RC (geomean)</text>')
    o.append(f'<text x="14" y="{(H - B) / 2}" text-anchor="middle" class="tick" transform="rotate(-90 14 {(H - B) / 2})">time ÷ RC (geomean)</text>')
    for v in sorted(set(round(t, 1) for t in (x0, (x0 + x1) / 2, x1))):
        o.append(f'<text x="{X(v):.1f}" y="{H - B + 16}" text-anchor="middle" class="tick">{v:.1f}×</text>')
    for v in sorted(set(round(t, 2) for t in (y0, (y0 + y1) / 2, y1))):
        o.append(f'<text x="{L - 6}" y="{Y(v) + 4:.1f}" text-anchor="end" class="tick">{v:.2f}×</text>')
    line = " ".join(f"{X(p[2]):.1f},{Y(p[1]):.1f}" for p in pts)
    o.append(f'<polyline points="{line}" fill="none" class="gcline"/>')
    for f, t, m, _ in pts:
        o.append(f'<circle cx="{X(m):.1f}" cy="{Y(t):.1f}" r="5" class="lose"><title>GC grow {f:g}×: time {t:.2f}×, RSS {m:.2f}× RC</title></circle>'
                 f'<text x="{X(m) + 8:.1f}" y="{Y(t) - 6:.1f}" class="lbl">grow {f:g}×</text>')
    o.append(f'<circle cx="{X(1):.1f}" cy="{Y(1):.1f}" r="6" class="win"><title>RC + arenas</title></circle><text x="{X(1) + 8:.1f}" y="{Y(1) + 16:.1f}" class="lbl">RC</text>')
    if d:
        o.append(f'<circle cx="{X(d[1]):.1f}" cy="{Y(d[0]):.1f}" r="6" class="win"><title>RC, deferred: time {d[0]:.2f}×, RSS {d[1]:.2f}× RC</title></circle><text x="{X(d[1]) + 8:.1f}" y="{Y(d[0]) + 16:.1f}" class="lbl">RC, deferred</text>')
    o.append("</svg>")
    rows = [[f"GC, grow {f:g}×", fmt(t, 2) + "×", fmt(m, 2) + "×", fmt(h, 2) + "×"] for f, t, m, h in pts]
    if d:
        rows.append(["RC, deferred", fmt(d[0], 2) + "×", fmt(d[1], 2) + "×", fmt(geo_ratio("rc-deferred", "rc", "heap"), 2) + "×"])
    return ("<section><h2>Time–space tradeoff</h2><p>The tracing GC at several heap growth factors "
            "(collect when the heap reaches that multiple of what survived the last collection), "
            "against RC + arenas at 1×, 1×. Below and left of RC: the GC is better on both.</p>"
            + "".join(o) + table(["configuration", "time ÷ RC", "peak RSS ÷ RC", "peak heap ÷ RC"], rows) + "</section>")


def rel(b, c, key):
    r, g = by.get((b, "rc")), by.get((b, "gc"))
    return r[key] / g[key] if r and g and r.get(key) and g.get(key) else None


# Thesis section: RC + arenas against the tracing GC.
time_r = geo_ratio("rc", "gc")
rss_r = geo_ratio("rc", "gc", "rss")
pause_rc = max((by[(b, "rc")].get("pause_max") or 0) for b in benches if (b, "rc") in by)
pause_gc = max((by[(b, "gc")].get("pause_max") or 0) for b in benches if (b, "gc") in by)
wins_t = sum(1 for b in benches if (rel(b, "rc", "wall") or 9) < 1)
wins_m = sum(1 for b in benches if (rel(b, "rc", "rss") or 9) < 1)

thesis_rows = []
for b in benches:
    r, g = by.get((b, "rc"), {}), by.get((b, "gc"), {})
    tr, mr = rel(b, "rc", "wall"), rel(b, "rc", "rss")
    dr = by.get((b, "rc-deferred"), {}).get("wall")
    dg = dr / g["wall"] if dr and g.get("wall") else None
    thesis_rows.append([esc(b), fmt(r.get("wall")), fmt(dr), fmt(g.get("wall")),
                        f'<span class="{"w" if dg and dg < 1 else "l"}">{fmt(dg, 2)}×</span>',
                        f'<span class="{"w" if tr and tr < 1 else "l"}">{fmt(tr, 2)}×</span>',
                        fmt(r.get("rss"), 0), fmt(g.get("rss"), 0),
                        f'<span class="{"w" if mr and mr < 1 else "l"}">{fmt(mr, 2)}×</span>',
                        fmt(r.get("pause_max"), 2), fmt(g.get("pause_max"), 2)])

all_rows = []
for b in benches:
    all_rows.append([esc(b)] + [fmt(by[(b, c)]["wall"]) if (b, c) in by else "—" for c in configs])
all_rows.append(["<b>geomean</b>"] + [f"<b>{fmt(geo(c))}</b>" for c in configs])

mem_rows = []
for b in benches:
    mem_rows.append([esc(b)] + [fmt(by[(b, c)]["rss"], 0) if (b, c) in by else "—" for c in configs]
                    + [fmt(by.get((b, "rc"), {}).get("heap"), 2), fmt(by.get((b, "gc"), {}).get("heap"), 2)])

lat = load("latency.json") or []
lat_rows = [[esc(l["config"]), fmt(l["mean_us"], 1), fmt(l["p50_us"], 1), fmt(l["p99_us"], 1), fmt(l["p999_us"], 1),
             fmt(l["max_us"], 1), fmt(l["rss_mb"], 1)] for l in lat]

hist_rows = [[esc(h["date"]), f'<code>{esc(h["commit"])}</code>', esc(h["step"]), fmt(h["rc_geomean_s"], 4),
              fmt(h["gc_geomean_s"], 4), fmt(h["rc_geomean_s"] / h["gc_geomean_s"], 2) + "×",
              fmt(h.get("jitless_geomean_s"), 4)] for h in reversed(hist)]

prof = load("profile.json")
prof_rows = []
if prof:
    top = sorted((s for s in prof["suite"] if s["instruction"] != "<outside>"), key=lambda s: -s["share"])[:20]
    prof_rows = [[f'<code>{esc(s["instruction"])}</code>', fmt(100 * s["share"], 1), f'{s["count"]:,}', f'{s["allocs"]:,}',
                  fmt(s["rc"] / s["count"] if s["count"] else 0, 2)] for s in top]

# The thesis measurements: memory over time and pauses (bench/memory.py).
mem = load("memory.json") or []
MCFG = [("rc", "RC + arenas", "win"), ("gc", "GC, default", "lose"), ("gc-small", "GC, tuned small", "mid"), ("rc-owners", "RC, ownership", "own"), ("rc-deferred", "RC, deferred", "alt")]
mby = {(r["bench"], r["config"]): r for r in mem}
mbench = list(dict.fromkeys(r["bench"] for r in mem))


def timeline_svg(b):
    W, H, L, B = 300, 130, 34, 18
    runs = [(c, n, k, mby[(b, c)]) for c, n, k in MCFG[:4] if (b, c) in mby]
    if not runs:
        return ""
    tmax = max(r["timeline"][-1][0] for *_, r in runs) or 1
    ymax = max(r["peak_mb"] for *_, r in runs) or 1
    X = lambda t: L + t / tmax * (W - L - 6)
    Y = lambda v: H - B - v / ymax * (H - B - 8)
    o = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(b)} memory over time">']
    o.append(f'<line x1="{L}" x2="{W - 6}" y1="{H - B}" y2="{H - B}" class="grid"/>')
    o.append(f'<text x="{L - 4}" y="{Y(ymax) + 4:.1f}" text-anchor="end" class="tick">{ymax:.0f}</text><text x="{L - 4}" y="{H - B + 4}" text-anchor="end" class="tick">0</text>')
    o.append(f'<text x="{W - 6}" y="{H - 4}" text-anchor="end" class="tick">{tmax / 1000:.1f} s</text><text x="{L}" y="12" class="lbl">{esc(b)} <tspan class="tick">MB live</tspan></text>')
    for c, n, k, r in reversed(runs):
        pts = " ".join(f"{X(t):.1f},{Y(v):.1f}" for t, v in r["timeline"])
        o.append(f'<polyline points="{pts}" fill="none" class="ln-{k}"><title>{esc(n)}: peak {r["peak_mb"]:.1f} MB, mean {r["mean_mb"]:.1f} MB</title></polyline>')
    o.append("</svg>")
    return "".join(o)


def mem_section():
    if not mem:
        return ""
    legend = "".join(f'<span><i class="sw-{k}"></i>{esc(n)}</span>' for c, n, k in MCFG[:4])
    charts = "".join(f'<div class="tl">{timeline_svg(b)}</div>' for b in mbench)
    rows = []
    for b in mbench:
        cells = [esc(b)]
        rc = mby.get((b, "rc"))
        for key, d in (("mean_mb", 2), ("peak_mb", 2)):
            for c, *_ in MCFG:
                r = mby.get((b, c))
                v = r[key] if r else None
                cls = "" if not (r and rc) or c == "rc" else (' class="l"' if v > rc[key] * 1.05 else (' class="w"' if v < rc[key] * 0.95 else ""))
                cells.append(f"<span{cls}>{fmt(v, d)}</span>")
        rows.append(cells)
    head = ["benchmark"] + [f"mean MB, {n}" for _, n, _ in MCFG] + [f"peak MB, {n}" for _, n, _ in MCFG]
    prow = []
    for b in mbench:
        cells = [esc(b)]
        for c, *_ in MCFG:
            r = mby.get((b, c))
            if not r:
                cells.append("—")
                continue
            worst = max(r["by_kind"].items(), key=lambda kv: kv[1]["max_ms"], default=None)
            cause = f' <span class="tick">({worst[0]})</span>' if worst and r["pause_max_ms"] > 0 else ""
            cells.append(f'{fmt(r["pause_max_ms"], 2)}{cause}')
        for c, *_ in MCFG:
            r = mby.get((b, c))
            cells.append(fmt(r["pause_p99_ms"], 2) if r else "—")
        prow.append(cells)
    phead = ["benchmark"] + [f"max ms, {n}" for _, n, _ in MCFG] + [f"p99 ms, {n}" for _, n, _ in MCFG]
    return (f'<section><h2>Memory over time</h2><p>Live heap above what was live at start, sampled every 250 µs (bench/memory.py; native code compiled up front, 5 iterations). Reference counting frees as it goes; a tracing GC lets garbage build up to a threshold, then collects. "Tuned small": the GC set to collect from 1 MB and grow 1.5×, its best memory setting.</p>'
            f'<div class="legend">{legend}</div><div class="tls">{charts}</div>'
            + table(head, rows, "mem") + "</section>"
            + f'<section><h2>Pauses</h2><p>Every pause, by what caused it: cycles (RC\'s cycle collector), gc (a tracing collection), settle (deferred RC), free (RC freeing a structure whose last reference went away, logged when 20 µs or longer). Longest and 99th percentile, ms.</p>'
            + table(phead, prow, "mem") + "</section>")


def mem_ratio(c, key):
    xs = [mby[(b, c)][key] / mby[(b, "rc")][key] for b in mbench if (b, c) in mby and (b, "rc") in mby and mby[(b, "rc")][key] > 0]
    return math.exp(sum(map(math.log, xs)) / len(xs)) if xs else None


def thesis_tiles():
    if not mem:
        return ""
    g = mem_ratio("gc", "mean_mb")
    gs = mem_ratio("gc-small", "mean_mb")
    rcmax = {b: mby[(b, "rc")] for b in mbench if (b, "rc") in mby}
    acyc = [r["pause_max_ms"] for r in rcmax.values() if "cycles" not in r["by_kind"] or r["by_kind"]["cycles"]["max_ms"] < r["pause_max_ms"]]
    noncyc = max((max([v["max_ms"] for k, v in r["by_kind"].items() if k != "cycles"], default=0) for r in rcmax.values()), default=0)
    cyc = max((r["by_kind"].get("cycles", {}).get("max_ms", 0) for r in rcmax.values()), default=0)
    gcmax = max((mby[(b, "gc-small")]["pause_max_ms"] for b in mbench if (b, "gc-small") in mby), default=0)
    lr, lg, ls = lat("RC + arenas"), lat("tracing GC"), lat("tracing GC, tuned small")
    tl = (f'<div class="tile"><div class="k">Request latency, p99.9</div><div class="v">{lr["p999_us"]:.0f} / {lg["p999_us"]:.0f} µs</div><div class="n">RC / GC default (tuned small: {ls["p999_us"]:.0f} µs)</div></div>' if lr and lg and ls else "")
    return (f'<section class="tiles">'
            f'<div class="tile"><div class="k">Average memory, GC ÷ RC</div><div class="v">{g:.1f}×</div><div class="n">geomean; GC tuned small: {gs:.1f}×</div></div>'
            f'<div class="tile"><div class="k">Longest RC pause, freeing</div><div class="v">{noncyc:.2f} ms</div><div class="n">any benchmark, excluding the cycle collector</div></div>'
            f'<div class="tile"><div class="k">Longest RC cycle collection</div><div class="v">{cyc:.1f} ms</div><div class="n">vs longest GC pause {gcmax:.1f} ms (tuned small)</div></div>'
            + tl + "</section>")


def lat(cfg):
    return next((l for l in (load("latency.json") or []) if l["config"] == cfg), None)


commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
date = subprocess.run(["git", "log", "-1", "--format=%cs"], capture_output=True, text=True).stdout.strip()
verdict_time = "faster" if time_r < 1 else "slower"

page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>rts: RC vs GC</title>
<style>
:root {{ --bg:#fbfaf7; --fg:#1d1c1a; --muted:#6b6862; --line:#e4e1da; --panel:#fff; --win:#2f7d5b; --lose:#b5523b; --mid:#c99a2e; --own:#3a6db0; --grid:#d8d4cc; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#191816; --fg:#ece9e3; --muted:#a29e96; --line:#34322e; --panel:#22211e; --win:#5cbf8e; --lose:#e07a60; --mid:#e0b955; --own:#7fa9e6; --grid:#3d3a35; }} }}
body {{ background:var(--bg); color:var(--fg); font:15px/1.5 -apple-system, system-ui, sans-serif; margin:0; padding:32px 16px; }}
main {{ max-width:980px; margin:0 auto; display:grid; gap:36px; }}
h1 {{ font:400 30px/1.2 Georgia, serif; margin:0; }} h2 {{ font-size:18px; margin:0 0 6px; }}
p {{ margin:0 0 8px; color:var(--muted); max-width:72ch; }}
.tiles {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(170px,1fr)); gap:12px; }}
.tile {{ background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:14px 16px; }}
.tile .k {{ font-size:12px; color:var(--muted); }} .tile .v {{ font:400 26px Georgia, serif; font-variant-numeric:tabular-nums; }}
.tile .n {{ font-size:12px; color:var(--muted); }}
.wrap {{ overflow-x:auto; }}
table {{ border-collapse:collapse; width:100%; font-size:13px; font-variant-numeric:tabular-nums; }}
th, td {{ padding:6px 10px; border-bottom:1px solid var(--line); text-align:right; white-space:nowrap; }}
th:first-child, td:first-child {{ text-align:left; }} th {{ color:var(--muted); font-weight:500; }}
.hist td:nth-child(3) {{ white-space:normal; text-align:left; min-width:260px; }}
.w {{ color:var(--win); font-weight:600; }} .l {{ color:var(--lose); font-weight:600; }}
svg {{ width:100%; max-width:640px; display:block; }} svg text {{ fill:var(--muted); font-size:12px; }}
svg .lbl {{ fill:var(--fg); }} svg .grid {{ stroke:var(--grid); }} svg .one {{ stroke:var(--muted); }}
svg .ln-win {{ stroke:var(--win); stroke-width:1.6; }} svg .ln-lose {{ stroke:var(--lose); stroke-width:1.2; }} svg .ln-mid {{ stroke:var(--mid); stroke-width:1.2; }} svg .ln-own {{ stroke:var(--own); stroke-width:1.6; }}
.tls {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(290px,1fr)); gap:14px; }} .tl svg {{ max-width:none; }}
.legend {{ display:flex; gap:16px; flex-wrap:wrap; font-size:12px; color:var(--muted); margin:4px 0 10px; }} .legend i {{ display:inline-block; width:14px; height:3px; margin-right:6px; vertical-align:middle; }}
.sw-win {{ background:var(--win); }} .sw-lose {{ background:var(--lose); }} .sw-mid {{ background:var(--mid); }} .sw-own {{ background:var(--own); }} .tick {{ color:var(--muted); font-weight:400; }}
table.mem th {{ white-space:normal; min-width:70px; }}
svg .win {{ fill:var(--win); }} svg .lose {{ fill:var(--lose); }} svg .gcline {{ stroke:var(--lose); stroke-width:1.5; }}
code {{ font-size:12px; }} footer {{ font-size:12px; color:var(--muted); }}
</style></head><body><main>
<header><h1>rts: reference counting vs tracing GC</h1>
<p><b>Thesis:</b> freeing memory as soon as it is dead (scope arenas, reference counting) keeps memory use close to what the program needs, steady and predictable, without collection pauses, where a tracing GC trades memory and pauses for throughput. The same engine and compiled code, built two ways: reference counting with scope arenas and a cycle collector (the default), and a tracing mark-sweep GC (<code>--features gc</code>). Typed Are-We-Fast-Yet ports and a request loop; output matches Node byte for byte. Commit <code>{esc(commit)}</code>, {esc(date)}.</p></header>

{thesis_tiles()}

{mem_section()}

{"<section><h2>Latency</h2><p>Request-style workload (bench/latency.py, native code compiled up front): per-request time in µs. A collection pause shows up in the tail.</p>" + table(["config", "mean", "p50", "p99", "p99.9", "max", "RSS MB"], lat_rows) + "</section>" if lat_rows else ""}

<section><h2>Throughput</h2><p>Secondary to the thesis, but a cost to know: the same programs' run time.</p></section>
<section class="tiles">
<div class="tile"><div class="k">Time, RC ÷ GC (geomean)</div><div class="v">{time_r:.2f}×</div><div class="n">RC {verdict_time}; RC wins {wins_t} of {len(benches)}</div></div>
<div class="tile"><div class="k">Peak RSS, RC ÷ GC (geomean)</div><div class="v">{rss_r:.2f}×</div><div class="n">RC smaller on {wins_m} of {len(benches)}</div></div>
<div class="tile"><div class="k">Longest pause</div><div class="v">{pause_rc:.1f} / {pause_gc:.0f} ms</div><div class="n">RC / GC, worst benchmark</div></div>
<div class="tile"><div class="k">Suite geomean time</div><div class="v">{geo("rc"):.3f} / {geo("gc"):.3f} s</div><div class="n">RC / GC</div></div>
</section>

<section><h2>Time, RC ÷ GC</h2><p>Below 1×: reference counting is faster. Log scale.</p>
{ratio_bars([(b, rel(b, "rc", "wall")) for b in benches], "time ratio")}</section>

<section><h2>Peak memory, RC ÷ GC</h2><p>Process peak RSS. Below 1×: reference counting uses less. The GC collects once the heap reaches 8 MB, then each time it doubles what survived.</p>
{ratio_bars([(b, rel(b, "rc", "rss")) for b in benches], "memory ratio")}</section>

{tradeoff()}

<section><h2>Per benchmark</h2><p>Seconds (each harness's own measurement, start-up excluded, median of runs), peak RSS in MB, longest collector pause in ms.</p>
{table(["benchmark", "RC s", "deferred s", "GC s", "deferred÷GC", "time RC÷GC", "RC MB", "GC MB", "RSS RC÷GC", "RC pause", "GC pause"], thesis_rows)}</section>

<section><h2>Every configuration: time (s)</h2>
{table(["benchmark"] + [LABEL[c] for c in configs], all_rows)}</section>

<section><h2>Every configuration: peak RSS (MB)</h2><p>Last two columns: peak live heap (MB) as rts counts it.</p>
{table(["benchmark"] + [LABEL[c] for c in configs] + ["RC heap", "GC heap"], mem_rows)}</section>


<section><h2>History</h2><p>Suite geometric means at each recorded step (newest first).</p>
{table(["date", "commit", "step", "RC s", "GC s", "RC÷GC", "node --jitless s"], hist_rows, "hist")}</section>

{"<section><h2>Instruction profile (interpreter, profiling build)</h2><p>Share of sampled time, executions, allocations and reference-count updates per execution.</p>" + table(["instruction", "time %", "count", "allocs", "rc ops/op"], prof_rows) + "</section>" if prof_rows else ""}

<footer>Generated by bench/results/build.py from bench/memory.py, bench/latency.py, bench/modes.py and bench/profile.py (bench/measure.sh runs them all).</footer>
</main></body></html>
"""
open(os.path.join(HERE, "report.html"), "w").write(page)
print({c: round(geo(c), 4) for c in configs}, "->", os.path.join(HERE, "report.html"))
