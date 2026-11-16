#!/bin/bash
# Full measurement for the bench page, from snapshots of the three builds (so
# rebuilding during the run can't mix binaries), then the page data:
#   bench/measure.sh "what changed in this step"
set -e
cd "$(dirname "$0")/.."
SNAP=$(mktemp -d)
cp target/release/rts "$SNAP/rts"; cp target/gc/release/rts "$SNAP/rts-gc"; cp target/profile/release/rts "$SNAP/rts-profile"
export RTS="$SNAP/rts" RTS_GC="$SNAP/rts-gc" RTS_PROFILE="$SNAP/rts-profile"
SUITE=ts RUNS=3 CONFIGS=rc,rc-noarena,rc-nocc,gc,node,jitless OUT=bench/results/modes_ts.json python3 bench/modes.py > /dev/null 2>&1
OUT=bench/results/profile.json python3 bench/profile.py > /dev/null 2>&1
python3 bench/latency.py > /dev/null 2>&1
python3 bench/results/build.py --step "$1"
rm -rf "$SNAP"
