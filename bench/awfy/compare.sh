#!/bin/bash
# Run the Are-We-Fast-Yet benchmarks under rts and node and compare output
# (timings stripped). usage: [SIZE=full] bench/awfy/compare.sh [rts flags...]
cd "$(dirname "$0")/js"
RTS="$(cd ../../.. && pwd)/target/release/rts"
full() { case $1 in DeltaBlue) echo 12000;; Richards|Json) echo 100;; CD) echo 250;; Havlak|Bounce|List) echo 1500;; Mandelbrot) echo 500;; NBody) echo 250000;; Permute|Queens|Storage) echo 1000;; Sieve) echo 3000;; Towers) echo 600;; esac; }
test_size() { case $1 in CD) echo 10;; *) echo 1;; esac; }
pass=0; fail=0
for B in Bounce CD DeltaBlue Havlak Json List Mandelbrot NBody Permute Queens Richards Sieve Storage Towers; do
  if [ "$SIZE" == "full" ]; then n=$(full $B); else n=$(test_size $B); fi
  strip() { sed -E 's/[0-9]+us/Nus/g'; }
  exp=$(node harness.js $B 1 $n 2>&1 | strip)
  got=$(timeout 600 "$RTS" "$@" harness.js $B 1 $n 2>&1 | strip)
  if [ "$exp" == "$got" ]; then echo "PASS $B"; pass=$((pass+1)); else echo "FAIL $B"; echo "$got" | head -4; fail=$((fail+1)); fi
done
echo "passed $pass, failed $fail"
