#!/bin/bash
# A/B timing of two rts binaries on the typed AWFY suite (best of N harness runs).
#   bench/ab.sh <baseline-rts> <candidate-rts> [Bench,Bench,...]
# N defaults to 3 (env N).
cd "$(dirname "$0")/awfy/ts"
A=$1; B=$2; LIST=${3:-Bounce,CD,DeltaBlue,Havlak,Json,List,Mandelbrot,NBody,Permute,Queens,Richards,Sieve,Storage,Towers}
size() { case $1 in DeltaBlue) echo 12000;; Richards) echo 20;; Json) echo 100;; CD) echo 250;; Havlak) echo 1;; Bounce|List) echo 1500;; Mandelbrot) echo 500;; NBody) echo 250000;; Permute|Queens|Storage) echo 1000;; Sieve) echo 3000;; Towers) echo 600;; esac; }
best() { local b=999999999; for i in $(seq ${N:-3}); do t=$("$1" harness.ts $2 1 $(size $2) | sed -n 's/.*runtime: \([0-9]*\)us.*/\1/p' | head -1); [ "$t" -lt "$b" ] && b=$t; done; echo $b; }
prod=1
for X in ${LIST//,/ }; do
  a=$(best "$A" $X); b=$(best "$B" $X)
  r=$(python3 -c "print(f'{$b/$a:.3f}')")
  printf "%-11s %10d %10d  %s\n" $X $a $b $r
done
