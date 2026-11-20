#!/bin/bash
# Full measurement for the bench page, from snapshots of the four builds (so
# rebuilding during the run can't mix binaries), then the page data:
#   bench/measure.sh "what changed in this step"
set -e
cd "$(dirname "$0")/.."
SNAP=$(mktemp -d)
cp target/release/rts "$SNAP/rts"; cp target/gc/release/rts "$SNAP/rts-gc"; cp target/profile/release/rts "$SNAP/rts-profile"
cp target/owners/release/rts "$SNAP/rts-owners"
export RTS="$SNAP/rts" RTS_GC="$SNAP/rts-gc" RTS_PROFILE="$SNAP/rts-profile" RTS_OWNERS="$SNAP/rts-owners"
SUITE=ts RUNS=3 CONFIGS=rc,rc-owners,rc-deferred,rc-noarena,rc-nocc,gc,gc-1.25,gc-1.5,gc-3,node,jitless OUT=bench/results/modes_ts.json python3 bench/modes.py > /dev/null 2>&1
OUT=bench/results/profile.json python3 bench/profile.py > /dev/null 2>&1
python3 bench/latency.py > /dev/null 2>&1
python3 bench/memory.py > /dev/null 2>&1
python3 bench/results/build.py --step "$1"
rm -rf "$SNAP"
