"""Minimum mutator utilization: over every window of a given length, the
smallest fraction of it the program ran (wasn't paused by memory
management). Short pauses packed close together show here even when the
longest pause looks fine.

    python3 bench/mmu.py                     # prints a table
    BENCH=Havlak CONFIGS=rc-owners python3 bench/mmu.py

Uses the same programs and configurations as bench/memory.py, and the
pauses RTS_MEM_TRACE logs (start time and length of each).
"""
import json, os, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from memory import CONFIGS, PROGRAMS  # noqa: E402

WINDOWS_MS = [1, 2, 10, 50]


def pauses(cmd, cwd):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
        path = f.name
    env = dict(os.environ, RTS_JIT_EAGER="1", RTS_MEM_TRACE=path)
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=env)
    if p.returncode != 0:
        raise RuntimeError(p.stderr[-2000:])
    d = json.load(open(path))
    os.unlink(path)
    end = d["samples"][-1][0] if d["samples"] else 0
    return [(t, t + ns / 1000) for t, ns, _ in d["pauses"]], end


def mmu(ps, end, w):
    """Worst window: the paused time in [s, s + w] is largest when the
    window starts at a pause's start or ends at a pause's end."""
    if end <= w:
        return None
    starts = [s for s, _ in ps] + [e - w for _, e in ps]
    worst = 0.0
    j0 = 0
    for s in sorted(x for x in starts if 0 <= x <= end - w):
        while j0 < len(ps) and ps[j0][1] <= s:
            j0 += 1
        busy, j = 0.0, j0
        while j < len(ps) and ps[j][0] < s + w:
            busy += min(ps[j][1], s + w) - max(ps[j][0], s)
            j += 1
        worst = max(worst, busy)
    return 1 - worst / w


def main():
    want = os.environ.get("BENCH")
    configs = os.environ.get("CONFIGS", "rc,rc-owners,gc,gc-small").split(",")
    print(f"{'program':10} {'config':10} " + " ".join(f"{'MMU ' + str(w) + 'ms':>9}" for w in WINDOWS_MS))
    for name, (cwd, args) in PROGRAMS.items():
        if want and name not in want.split(","):
            continue
        for c in configs:
            try:
                ps, end = pauses(CONFIGS[c] + args, cwd)
            except Exception as e:  # noqa: BLE001
                print(f"{name:10} {c:10} FAILED {e}", file=sys.stderr)
                continue
            ps.sort()
            row = [mmu(ps, end, w * 1000) for w in WINDOWS_MS]
            print(f"{name:10} {c:10} " + " ".join(f"{'-' if v is None else f'{v * 100:.0f}%':>9}" for v in row))


if __name__ == "__main__":
    main()
