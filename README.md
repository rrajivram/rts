# rts

rts is a TypeScript runtime written in Rust. It has **no tracing garbage
collector**: memory is freed by scope where the compiler can prove it, by
reference counting elsewhere, and by a cycle collector for garbage cycles. It
also **uses static types to choose storage and instructions ahead of time**.

The project tests a thesis: *scope-based memory management beats a tracing
GC.* The same engine can be built with a tracing GC instead, so the
comparison changes nothing but the memory strategy. See
[Memory: does scope-based management pay off?](#memory-does-scope-based-management-pay-off).

This is a proof of concept. It runs TypeScript (and plain JavaScript)
directly from source, with no transpile step: ES modules and CommonJS,
generators, async/await with Node's event loop ordering, regular
expressions, and the commonly used parts of Node's built-in modules. The
long-term goal is to run arbitrary npm packages.

```sh
cargo build --release
./target/release/rts program.ts [args…]   # run (imports are loaded from disk)
./target/release/rts --dump program.ts    # show the typed bytecode
./target/release/rts --no-types program.ts  # ignore all type information (experiment)
tests/run.sh                              # conformance tests (compared against Node)
bench/run.py                              # benchmarks vs node, node --jitless, rjs
bench/types.py                            # typed vs --no-types on the same engine
bench/awfy/compare.sh                     # Are-We-Fast-Yet suite: output must match Node
python3 bench/modes.py                    # memory modes: arenas / RC / GC / Node
```

Memory flags: `--mem-stats` (heap, arena and collector statistics on exit),
`--no-arena` (no scope arenas), `--no-cycles` (no cycle collector). The
tracing-GC build is `cargo build --release --features gc --target-dir target/gc`.

## How it works

```
lexer → parser → type checker → typed HIR → codegen → instructions on typed slots → VM
```

See [DESIGN.md](DESIGN.md) for details. In short:

- **Typed slots.** Every variable, parameter and temporary lives in a numbered
  *slot* of its function's frame, and the compiler fixes each slot's type:
  a number slot holds a raw `f64`, a boolean slot holds 0 or 1, a bigint
  takes two number slots (an `i128`), and a value slot holds a tagged `Value`
  (strings, objects, `any`…). Numbers, booleans and bigints never touch the
  heap or a refcount. When the types are known, `a + b`
  compiles to `Arithmetic { op: Add, left: a, right: b, output: … }` on number
  slots: a plain f64 add with no tag check.
- **Hybrid typing.** `any`, `unknown`, unions and generics fall back to boxed
  values with dynamic operations. At each boundary the checker inserts a
  *checked* unbox, so an incorrect `as`, a bad `.d.ts` or a covariant array
  write raises a `TypeError` instead of corrupting memory (see `tests/rts/`).
- **No GC.** Heap values are `Rc`, freed deterministically the moment they
  die. Frames release their references when they return, and blocks release
  theirs when they exit. Reference cycles leak (see Next steps).
- **Objects.** Class instances have fixed layouts, so typed field access is a
  fixed position (`ReadFieldNumber`). Method calls go through a vtable.
  Structural types and `any` use shape-keyed inline caches.
- **Arrays.** `number[]` uses `Vec<f64>` storage. It degrades to `Vec<Value>`
  if something writes a non-number into it through `any`.
- **Modules.** The checker is whole-program: every module is a scope, and an
  import binds the importing scope's name to the exporting module's own
  variable. Imports are live bindings with no run-time lookup, and a typed
  export stays typed across files.
- **Generators and async functions** run in ordinary frames. `yield` and
  `await` move the frame's slots into a heap `Coroutine`, and resuming moves
  them back, so a generator's `number` locals stay unboxed while it is
  suspended. Promises, microtasks and timers follow the ECMAScript job model:
  callback order matches Node.

### Optimizations that rely on knowing types

- Every variable is resolved to a slot at compile time. Top-level variables
  used only by top-level code live in the top-level code's slots; the rest
  are typed globals.
- Numeric constants used in loops are loaded into slots once (`#3` in dumps),
  and `const x = 5` reads are folded to the literal.
- Fused compare-and-branch (`JumpIfCompare`), rotated loops (one branch per
  iteration), `AddConstant` for `x + k`, an integer fast path for `%`, and a
  peephole pass that removes jumps to the next instruction.
- `arr.map/filter/forEach/reduce/some/every/find/findIndex` compile to
  bytecode loops. Arrow callbacks are **inlined** into the loop, so
  `xs.map(x => x * 2)` on a `number[]` runs about 4 instructions per element,
  with no call and no boxing.
- Hot built-ins (`Math.*`, `push` onto `number[]`, `charCodeAt`) are dedicated
  instructions.
- Call sites state the kinds of the values they pass. When those match the
  callee's parameter kinds (the usual case), arguments are copied from
  slot to slot. Otherwise the VM converts per argument, which is how
  calls through `any`, generics and callbacks to natives work.

## Benchmarks

These are wall-clock medians of 7 runs on Apple Silicon, using
`bench/run.py`. Node figures include about 46ms of process startup; rts starts
in about 2ms.

| program | rts | node --jitless | node (JIT) | rjs |
|---|---:|---:|---:|---:|
| complex (sieve, sort, stats, matrix, strings) | **8.7 ms** | 57.7 ms | 48.1 ms | 352.7 ms |
| complex, untyped original (`let x = []`) | 17.0 ms | 58.8 ms | 48.7 ms | 348.9 ms |
| fib(30), recursive calls | 55.5 ms | 86.2 ms | 50.5 ms | 1544.8 ms |
| array_sum (1M push + 10M indexed reads) | 63.1 ms | 188.2 ms | 62.6 ms | – |
| closures (map/filter/reduce over 1M, ×5) | 141.4 ms | 182.7 ms | 124.7 ms | – |
| loop (10M iterations, `%`-bound) | 110.3 ms | 129.1 ms | 75.4 ms | – |
| nbody (class fields, float math, 200k steps) | 229.5 ms | 464.2 ms | 104.9 ms | – |
| strings (join/split/Map/charCodeAt) | 45.6 ms | 106.3 ms | 78.6 ms | – |
| alloc_churn (2M short-lived objects) | 334.8 ms | 356.5 ms | 63.8 ms | – |
| bigint (modpow and a hash mod 2^61−1) | **67.6 ms** | 182.3 ms | 154.7 ms | – |

rts is faster than `node --jitless` on everything here, and on several
benchmarks it is within startup time of JIT Node. On `bigint` it beats JIT
Node: typed BigInts stay in slots as `i128`s, while V8 heap-allocates every
BigInt result (V8's are arbitrary precision; see BigInts below). Comparing the two `complex`
rows shows what type annotations are worth: about 2× on the same program.

**Memory (alloc_churn, peak RSS):** rts **3.3 MB** vs Node **54 MB**
(jitless: 48 MB). Without a collector, garbage never accumulates.

## Do types actually help? A controlled experiment

`rts --no-types` compiles the same source with every variable, parameter, return
value, field and `this` treated as `any`. Everything else stays the same: the
engine, the bytecode format, inline caches, element-kind arrays and the output
(checked byte for byte). The only thing removed is what the compiler knows
about types, so any difference comes from type knowledge.

To keep the untyped side from being a strawman, it has the standard tricks of a
good dynamic interpreter:

- shape-based inline caches for property reads, writes and method calls
- number-first array storage that degrades on the first non-number (V8's "elements kinds")
- pre-boxed constants
- integer fast paths for dynamic arithmetic
- allocation-free callbacks from built-ins

V8's interpreter (`node --jitless`) is shown for reference.

`bench/types.py` (median of 7 runs, Apple Silicon):

| program | typed | untyped | **types buy** | node --jitless |
|---|---:|---:|---:|---:|
| array_sum | 65.3 ms | 189.7 ms | **2.9×** | 188.9 ms |
| complex | 9.9 ms | 24.4 ms | **2.5×** | 57.8 ms |
| closures (map/filter/reduce) | 139.3 ms | 433.4 ms | **3.1×** | 183.7 ms |
| nbody | 240.5 ms | 629.7 ms | **2.6×** | 465.9 ms |
| strings | 45.2 ms | 108.0 ms | **2.4×** | 108.4 ms |
| fib | 62.2 ms | 116.5 ms | **1.9×** | 86.0 ms |
| loop | 80.4 ms | 191.5 ms | **2.4×** | 128.6 ms |
| complex_untyped (only inferred types) | 15.5 ms | 24.6 ms | **1.6×** | 58.7 ms |
| alloc_churn | 359.2 ms | 551.8 ms | **1.5×** | 361.7 ms |
| bigint | 65.4 ms | 177.2 ms | **2.7×** | 177.9 ms |

Types make every program faster, by 1.5× to 3.1×. The untyped engine is a
fair baseline: it is in the same range as V8's interpreter, faster on two
programs and up to about 2× slower on the rest. The typed engine beats V8's
interpreter on all of them.

Where the speedup comes from, using `fib` as the example (`rts --dump` vs
`rts --no-types --dump`). Slots are shown by variable name; `num3`, `bool0`
and `val5` are temporaries (number, boolean and value slots):

```
function fib(n: number)                               function fib(n: value)
    LoadNumber { value: 2, output: num2 }                 LoadConstant { constant: 2, output: val3 }
    JumpIfCompare { op: Lt, when: false,                  DynamicCompare { op: Lt, left: n, right: val3, output: bool0 }
                    left: n, right: num2, to: L4 }        JumpIf { when: false, condition: bool0, to: L5 }
    CopyNumber { from: n, to: num1 }                      CopyValue { from: n, to: val2 }
    Jump { to: L9 }                                       Jump { to: L14 }
L4:                                                   L5:
    AddConstant { left: n, constant: -1, output: num3 }   LoadConstant { constant: 1, output: val4 }
                                                          DynamicArithmetic { op: Sub, left: n, right: val4, output: num0 }
                                                          BoxNumber { value: num0, output: val5 }
    Call { function: fib, args: [num3], output: num2 }    Call { function: fib, args: [val5], output: val3 }
    …same for n - 2…                                      …same for n - 2…
    Arithmetic { op: Add, left: num2, right: num4,        DynamicAdd { left: val3, right: val6, output: val2 }
                 output: num1 }
L9:                                                   L14:
    CopyNumber { from: num1, to: num2 }                   CopyValue { from: val2, to: val3 }
    ReturnNumber { value: num2 }                          ReturnValue { value: val3 }
```

With types known, numbers stay in untagged number slots: no tag checks
(`DynamicCompare` and `DynamicAdd` check whether each side is a number or a
string), no boxing, and no reference-counted copies (`CopyValue`). The compiler can also do things that are impossible
without types:

- fuse a compare with its branch
- turn `obj.x` into a fixed slot read instead of a cache check
- compile `xs.map(x => x * 2)` into an inline loop instead of a callback per element

The last one is the 2.9× on `closures`.

## Memory: does scope-based management pay off?

**The thesis:** freeing memory by scope, falling back to reference counting
where scope can't be proven, beats a tracing garbage collector.

Results: open `bench/results/report.html` (self-contained, opens from disk). It is
regenerated after every step by `bench/measure.sh "what changed"` (`bench/modes.py`,
`bench/latency.py`, `bench/profile.py`, then `bench/results/build.py --step "…"`).

**Current standing (2026-11-17).** The thesis is about memory and pauses, not
throughput. `bench/memory.py` samples live heap every 250 µs and logs every pause
by cause; `bench/latency.py` times each request of a request loop.

- Memory: RC's average live heap is 1.0–8× below the GC's at its default setting
  and 1.3–6× below a GC tuned for small memory (Json 0.7 vs 4.2 MB, Storage 0.5
  vs 2.7, CD 1.3 vs 2.8, requests 2.3 vs 3.2), except on cyclic graphs (Havlak
  24 vs 24, DeltaBlue 11.6 vs 12.1).
- Pauses: RC freeing a dead structure never took more than 0.5 ms; the GC's
  pauses run 0.8–10 ms. Request latency p99.9: RC 27 µs, GC 1,983 µs (272 µs
  tuned small).
- The exception is cyclic garbage: RC's cycle collector pauses up to 11.6 ms on
  Havlak and 6.0 ms on DeltaBlue, as long as the GC's. That is the open problem.
- Throughput is about equal (GC 0.98× RC's time at default growth).
- Next: [docs/memory-model.md](docs/memory-model.md). A heap census shows tuning
  can't fix the cycle collector: 93–99.9% of its tracing starts at objects whose
  types really are cyclic. Today's RC holds 30–1300% more than an exact collector
  on cyclic programs. Grouping objects by *owner* (first referrer) matches exact
  memory within 6% on every program, with no tracing. The proposal is to replace
  the cycle collector with ownership: an orphan check whose cost is proportional
  to the garbage being freed, plus budgeted incremental freeing.
- Prototype (`cargo build --release --features owners --target-dir
  target/owners`, 2026-11-20). On the cyclic programs it has the lowest memory
  of any strategy, 1.3–3.8× below the GC tuned for small memory: Havlak 13.4
  MB mean / 15.7 peak, against 24.2 / 38.3 for today's RC and 20.8 / 35.6 for
  the GC. It ties today's RC on the acyclic ones. Longest pauses fall (Havlak
  14.7 → 3.7 ms), but DeltaBlue's is still 6.1 ms. Throughput costs 1.31×.
  Details in [docs/memory-model.md](docs/memory-model.md).

Measurements before 2026-11-17 that used native code are not a fair baseline:
the GC build barely collected while compiled code ran (950 MB peak on Havlak).
The numbers below are from the first, interpreter-only measurement.

### Method

**One engine, four memory strategies.** Only the memory strategy changes;
the instructions are identical.

| configuration | how memory is freed |
|---|---|
| **RC + arenas** (default) | scope arenas where escape analysis proves it, reference counting elsewhere, cycle collector for garbage cycles |
| **RC, no arenas** (`--no-arena`) | reference counting and cycle collector only |
| **RC, no cycle collector** (`--no-cycles`) | reference counting only (cycles leak) |
| **tracing GC** (`--features gc`) | mark-sweep; counts are not maintained at all |

**Workloads.**
- The Are-We-Fast-Yet suite, ported to typed TypeScript (`bench/awfy/ts`).
  The ports run on Node unchanged, and rts's output matches Node's
  byte-for-byte.
- A request-loop workload, `bench/workloads/requests.ts`.

**Measurement.**
- Time is the harness's own measurement, so start-up isn't included. Each
  figure is the median of 3 runs on an Apple M-series machine.
- RSS is the process maximum.
- Node (V8 with JIT) and `node --jitless` (V8's interpreter) are reference
  points only. They differ in far more than memory management.

To reproduce:

    cargo build --release && cargo build --release --features gc --target-dir target/gc
    SUITE=ts python3 bench/modes.py

### Results: time

| benchmark | objects | in arenas | RC+arenas | RC, no arenas | tracing GC | node --jitless | node (JIT) |
|---|--:|--:|--:|--:|--:|--:|--:|
| Bounce | 153,432 | 2% | 0.63 s | 0.63 s | 0.56 s | 0.54 s | 0.021 s |
| CD | 3,234,456 | 39% | 1.36 s | 1.39 s | 1.11 s | 0.76 s | 0.048 s |
| DeltaBlue | 1,081,066 | 0% | 0.38 s | 0.38 s | 0.32 s | 0.19 s | 0.037 s |
| Havlak | 10,068,269 | 0% | 1.97 s | 1.97 s | 1.61 s | 0.92 s | 0.091 s |
| Json | 3,331,032 | 0% | 0.78 s | 0.78 s | 0.69 s | 0.35 s | 0.030 s |
| List | 46,932 | 99% | 0.81 s | 0.82 s | 0.62 s | 0.34 s | 0.024 s |
| Mandelbrot | 432 | 0% | 0.18 s | 0.18 s | 0.18 s | 0.27 s | 0.018 s |
| NBody | 250,442 | 0% | 0.36 s | 0.35 s | 0.33 s | 0.57 s | 0.072 s |
| Permute | 1,433 | 0% | 0.86 s | 0.86 s | 0.66 s | 0.65 s | 0.029 s |
| Queens | 40,436 | 0% | 0.75 s | 0.75 s | 0.63 s | 0.42 s | 0.021 s |
| Richards | 1,392 | 0% | 0.71 s | 0.71 s | 0.56 s | 0.27 s | 0.014 s |
| Sieve | 3,432 | 0% | 0.66 s | 0.66 s | 0.57 s | 0.54 s | 0.037 s |
| Storage | 5,462,432 | 0% | 0.86 s | 0.86 s | 0.75 s | 0.35 s | 0.035 s |
| Towers | 9,433 | 0% | 0.91 s | 0.90 s | 0.72 s | 0.55 s | 0.026 s |

### Results: peak memory and pauses

| benchmark | RC+arenas | RC, no cycle collector | tracing GC | node --jitless | max pause RC | max pause GC |
|---|--:|--:|--:|--:|--:|--:|
| Bounce | 15 MB | 15 MB | 23 MB | 52 MB | 0.0 ms | 1.1 ms |
| CD | 17 MB | 130 MB | 23 MB | 60 MB | 1.1 ms | 1.2 ms |
| DeltaBlue | 42 MB | 37 MB | 51 MB | 79 MB | 5.5 ms | 3.3 ms |
| Havlak | 71 MB | 119 MB | 52 MB | 118 MB | 14.2 ms | 4.3 ms |
| Json | 17 MB | 16 MB | 24 MB | 52 MB | 0.1 ms | 1.8 ms |
| List | 15 MB | 15 MB | 21 MB | 47 MB | 0.0 ms | 0.0 ms |
| Mandelbrot | 15 MB | 15 MB | 15 MB | 48 MB | 0.0 ms | 0.0 ms |
| NBody | 15 MB | 15 MB | 23 MB | 47 MB | 0.0 ms | 1.3 ms |
| Permute | 15 MB | 15 MB | 15 MB | 46 MB | 0.0 ms | 0.0 ms |
| Queens | 16 MB | 15 MB | 22 MB | 48 MB | 0.1 ms | 0.7 ms |
| Richards | 15 MB | 15 MB | 15 MB | 47 MB | 0.0 ms | 0.0 ms |
| Sieve | 15 MB | 15 MB | 22 MB | 48 MB | 0.0 ms | 0.2 ms |
| Storage | 16 MB | 16 MB | 22 MB | 60 MB | 0.1 ms | 1.0 ms |
| Towers | 16 MB | 15 MB | 16 MB | 46 MB | 0.0 ms | 0.0 ms |

### Results: request loop (100,000 requests)

| configuration | mean | p50 | p99 | p99.9 | max | RSS |
|---|--:|--:|--:|--:|--:|--:|
| RC + arenas | 25.7 µs | 28.5 µs | 47 µs | **107 µs** | 140 µs | 12.8 MB |
| RC, no arenas | 25.5 µs | 28.3 µs | 47 µs | 105 µs | 134 µs | 12.7 MB |
| RC, no cycle collector | 25.7 µs | 28.7 µs | 47 µs | 54 µs | 136 µs | 10.8 MB |
| tracing GC | 25.6 µs | 23.5 µs | 40 µs | **1,424 µs** | 1,822–4,358 µs | 20.1 MB |
| node (JIT) | 7.8 µs | 8.6 µs | 15 µs | 26 µs | 152 µs | 97 MB |

In this workload, 5.6 million of the 31 million objects (18%) were allocated
in scope arenas, and none outlived its scope.

### What the numbers say

1. **Determinism wins on tail latency.** Average throughput is the same with
   either strategy. At p99.9, though, a request under the tracing GC
   sometimes waits through a collection: 1.4 ms, 13× worse than reference
   counting. With reference counting the slowest requests stay near
   100–140 µs, and the remaining tail comes from the cycle collector:
   without it, p99.9 is 54 µs.
2. **Reference counting uses less memory.** Peak RSS is 18% lower than the
   tracing GC across the suite (geometric mean), because garbage is freed
   immediately instead of when the heap has doubled. The exception is
   Havlak (71 MB against 52 MB), where the cycle collector's candidate buffer
   holds dead boxes until the next collection. rts as a whole uses 3–6× less
   memory than Node.
3. **Reference counting costs throughput.** Across the suite it is 18%
   slower than the tracing GC (geometric mean; 5–29% per benchmark). A
   profile of Havlak attributes about 19% of time to count updates when
   values are copied and dropped. The cycle collector adds 0–8%.
4. **Scope arenas, as implemented, have no measurable effect.** The run time
   ratio is 1.00 (geometric mean). Arenas take 99% of List's objects, 39% of
   CD's and 18% of the request loop's, yet time, peak memory and latency are
   unchanged. Two reasons:
   - Freeing an object whose count reaches zero is already cheap and
     immediate, so a bump-allocated arena saves only the malloc/free pair.
   - Count updates still happen on arena objects.

   The analysis also finds little to bind. Idiomatic code mostly stores new
   objects into fields and collections (`this.items.push(x)`,
   `node.next = n`), and the analysis can't prove those safe without
   ownership information. Factory methods (`a.minus(b)`) are handled by
   result regions; that is where CD's 39% comes from.

**Verdict, as of this commit.** The deterministic half of the thesis holds:
predictable latency and lower peak memory, at roughly 18% of throughput.
Scope-based freeing on top of reference counting does not yet pay for itself.
For it to matter, scope knowledge has to remove work that reference counting
does: skipping count updates for values the compiler knows are scope-bound,
and moving ownership of private fields into the owner's region. Those are
the next experiments (see [Next steps](#next-steps)).

## Native code (in progress)

Functions are compiled to machine code with [Cranelift](https://cranelift.dev)
(`src/jit/`), lazily: on a function's second call (its first, if it contains
a loop), together with the functions it calls directly. `RTS_JIT_EAGER=1`
compiles everything at start; `--no-jit` turns it off; `RTS_JIT_DEBUG=1`
lists what was compiled and which call sites still go through the
interpreter, and `RTS_JIT_WHY=<function>|*` says why a function wasn't
compiled. The AWFY harness starts in 42 ms (Node: 92 ms).

Numbers, booleans and class instances or arrays are held unboxed; any other
value (`T`, strings, unions, `any`) as its two words (tag, payload). Bodies
may use arithmetic, comparisons, control flow, `Math.*`, fields, elements,
properties by name, `obj[key]`, strings, globals, closures and captured
variables (in cells, as the interpreter keeps them), `new`, array literals
and `push`, `switch`, `for-of`, `throw` of any value, and untyped operators
(`+`, arithmetic and comparisons on `any`: numbers inline, the rest through
the interpreter's implementation), so untyped JavaScript compiles too. Not
compiled yet: `try`, object literals, generators and async functions.

Calls between compiled functions stay in compiled code:

- **Direct** calls, and methods whose class hierarchy shows one target.
- **Virtual** calls with several implementations compare the receiver's
  class and call the implementation (directly, or through a table of
  entry points when it is compiled later).
- **Function values**: a closure whose function is compiled with the kinds
  the call site passes is called directly (found by function id in the
  table); other kinds (generic callbacks) go through a per-function
  *adapter* that checks and unboxes value arguments inline.
- **By name** (interfaces, generics, untyped code): an inline cache per call
  site, keyed on the receiver's shape and class, calls the method's adapter.

Anything else (bound functions, built-ins, a mismatch) goes through the
interpreter, which enters compiled callees directly.

Typed suite geometric mean (one iteration, steady state): 0.108 s with
reference counting, 0.088 s with the tracing GC (`node --jitless`: 0.441 s).
The untyped JavaScript suite also runs compiled: 14% (NBody) to 58%
(Mandelbrot) faster than the interpreter, then 18–39% faster again with the
by-name call caches. See `docs/codebase-review.md` for the measurements
behind each step.

**Reference counts in compiled code** are maintained inline, with elision
rules: parameters borrow; a borrowed value used at once (a field read's base,
a comparison) takes no count; returning a local moves it; and in a *pure*
function (no field writes, only pure callees) nothing reachable from the
arguments can be freed, so variables that only hold such values, and the
arguments passed to pure calls, take no counts at all. On List this cuts
count updates from 26.0M to 1.27M (`--profile` builds report the total and
the updates per allocated box). Errors unwind to the interpreter and release
everything compiled code held.

Where reference counting loses to the tracing GC on compiled code (suite
geometric mean 0.123 s against 0.105 s), from `--profile` builds (which
report compiled code's count updates by source line) and sampling:

- **Count updates on stack references.** Richards makes 153M updates for
  5,482 objects; about 70% come from locals owning their values, `this` and
  arguments held across calls, and returned values. Only ~20% are field
  stores, which any reference counting must do.
- **Freeing objects one at a time.** On Storage (a tree of arrays, no
  cycles, almost no updates in compiled code) per-object `drop` and `free`
  are the difference; the tracing GC frees in bulk.
- **The cycle collector.** Turning it off saves Havlak 0.106 s of its
  0.222 s gap and CD 0.043 s of 0.073 s: trial deletion traces large live
  structures from each candidate.

Scope arenas now work in compiled code too: it allocates the same
scope-bound objects in the same arenas as the interpreter (CD 1.27M of
3.3M objects, List 99%), opening and leaving regions on every way out of a
block, and a function whose fresh result is only returned allocates it in
an arena its caller passes. With the region reset made allocation-free and
arenas keeping a chunk between uses, arenas make CD 1.7% and List 1%
faster than `--no-arena`. They save the `free` but not the count updates:
on CD only 3% of compiled code's count updates touch arena objects. So on
this suite arenas can't close the gap to the tracing GC.

Next: stack maps so the tracing GC can collect while compiled code runs,
fewer cycle-collector candidates, smaller objects (see
`docs/codebase-review.md` for the full list), then generators, async
functions and exceptions in compiled code. LLVM is planned as a later, more
optimizing tier.

## Supported TypeScript and Node

Everything in `tests/programs/` runs with output identical to Node
(`node --experimental-transform-types`), including uncaught-rejection and
microtask ordering.

- **Modules:** `import`/`export` in all forms (default, named, namespace,
  `export *`, `export * as ns`, re-exports, `import type`), cyclic imports,
  JSON modules, `import.meta`, dynamic `import()`, top-level `await`.
  Resolution follows Node: relative paths (a `./x.js` specifier finds
  `./x.ts`), `node_modules` with `package.json` `exports` (conditions and
  `./*` patterns), `module` and `main`. **CommonJS** works too: `require`,
  `module.exports`/`exports`, `__dirname`, CommonJS packages imported from
  ES modules. `.js` files are checked without types (everything is `any`).
  TypeScript `namespace` blocks become modules of their own.
- **Generators and async:** `function*`, `yield`, `yield*`, `return()`/
  `throw()` with `finally`, user iterables via `[Symbol.iterator]`, `for-of`
  with early-exit cleanup, spread and destructuring of iterables; `async`
  functions, methods and arrows, `await`, async generators and `for await`;
  `Promise` (`then`/`catch`/`finally`, `all`, `allSettled`, `race`, `any`,
  `withResolvers`), `queueMicrotask`, `setTimeout`/`setInterval`/
  `setImmediate`, `process.nextTick`, unhandled rejections (exit code 1).
- **Types:** primitives (including `bigint` and `symbol`), literal types,
  arrays, tuples, functions, object types, interfaces (recursive, generic,
  `extends`, merging), type aliases, unions, intersections, enums, classes
  and generics, `keyof`, `typeof`, indexed access `T[K]`, mapped types
  (`{ [K in keyof T]?: T[K] }`, with `+`/`-` modifiers), conditional types
  with `infer`, template literal types (as `string`), `Promise<T>`,
  `Generator`/`Iterable`/`AsyncIterable`, `Record`, `Partial`, `Required`,
  `Readonly`, `Pick`-style aliases, `Awaited`, `ReturnType`, type predicates
  and assertion functions. A deferred type (one that depends on unknown type
  parameters) behaves like `any` until its parameters are known.
- **Narrowing:** `typeof`, `===`/`!==` with `null`, `undefined` or literals,
  truthiness, `instanceof`, discriminant properties (`if`/`switch`, including
  `default`), `Array.isArray`, `in`, and narrowing after early `return`/`throw`.
- **Classes:** fields, `#private` fields, parameter properties, constructors,
  methods, getters and setters, `extends`/`super`, abstract classes, static
  members and static blocks, class expressions, virtual dispatch, `toString()`
  used by string conversion.
- **Functions:** closures with per-iteration loop bindings, arrows, defaults,
  optional and rest parameters, destructuring parameters, `arguments`, named
  function expressions, generic inference (including from callbacks and
  promises), `bind`/`call`/`apply`, tagged templates and `String.raw`.
- **Objects:** literals with getters/setters, computed keys, methods,
  spread, object rest destructuring, symbol keys.
- **Statements:** all loops, labels, `switch`, `try`/`catch`/`finally`
  (including `return` through `finally`), destructuring, spread, optional
  chaining (`a?.b`, `f?.()`), `??`/`??=`, template literals.
- **Built-ins:** `console` (`log`, `error`, `dir`, `group`, `count`, `time`,
  `table`, format strings like `%s %d %j`), `Math`, `JSON` (replacer,
  reviver, `toJSON`), `Object` (`keys`, `entries`, `assign`, `defineProperty`,
  `create`, `groupBy`, …), `Number`, `String`, `Array` (including `toSorted`,
  `with`, `findLast`), `Map`, `Set`, `WeakMap`/`WeakSet` (a Map/Set: without
  a tracing collector there is nothing weak references would add), `RegExp`
  (literals, named groups, lookaround, backreferences, `g`/`y`/`i`/`m`/`s`,
  via `fancy-regex`), `Date` (local time from the OS time zone),
  `Symbol`, `structuredClone`, `globalThis`, `encodeURIComponent` and
  friends, and the `Error` family with `cause` (written in TypeScript in
  `src/prelude.ts`).
- **Node:** `process` (`argv`, `env`, `exit`, `exitCode`, `cwd`, `platform`,
  `stdout.write`, `nextTick`, `hrtime.bigint`), and built-in modules written
  in TypeScript in `src/node/`: `node:fs` (synchronous API, `promises`, and
  callbacks; strings only, no `Buffer` yet), `node:path`, `node:events`
  (`EventEmitter`), `node:util` (`format`, `inspect`, `promisify`),
  `node:os`, `node:assert`, `node:url` (file URLs), `node:timers/promises`,
  `node:crypto` (`randomUUID`, non-cryptographic).
- **BigInts** are 128-bit integers (`i128`), as in rjs. That covers the
  usual uses (64-bit IDs, nanosecond timestamps, modular arithmetic) without
  a heap allocation per value. A result outside ±2^127 throws
  `RangeError: Maximum BigInt size exceeded`, where Node would keep growing.
  Typed `bigint` values are unboxed (`BigIntArithmetic` on two-slot
  `BigSlot`s); `any` values holding BigInts are boxed and checked.
- `console.log` formatting follows Node's `util.inspect` (line breaking and
  array grouping), so test output can be compared with Node byte for byte.

### Not supported yet

- Decorators, `enum`-less `declare` ambient modules' contents (they are
  skipped), `with`, `eval`/`new Function`, `Proxy`, `Reflect`, and
  property attributes (`enumerable: false`, real freezing).
- `Buffer`, `TextEncoder`/`TextDecoder`, `URL`, `fetch`, `http`, `net`,
  `child_process`, `worker_threads`, `Intl`.
- Stack traces: errors have no `stack` contents and `console.log(err)`
  prints `Name: message` without Node's `at …` lines.
- `yield*` inside async generators; `await` of `for await` iterator
  `return()` on early exit.
- Type-checking is deliberately lenient where TypeScript's rules are deep
  (overload resolution, variance, excess property checks). Unsoundness is
  caught at run time by checked unboxing instead.

## Next steps

For the memory thesis:

1. **Owned sub-objects.** A constructor's own allocations
   (`this.items = new Array()`) could live in the region of the object being
   built. The open problem is bounding growth when a long-lived object keeps
   replacing such a field.
2. **Container-aware analysis.** `Vector.append(x)` currently makes `x`
   escape: the analysis can't prove the backing array isn't shared. Ownership
   of private fields would fix that.
3. **Cheaper counting.** Most count traffic is slot-to-slot copies that the
   compiler could prove redundant (borrowing a value for the duration of a
   call).

For running npm packages:

1. **`Buffer`, `TextEncoder`, `URL`, `fetch`/`http`.** Most packages that do
   I/O need at least `Buffer`. `node:fs` currently reads and writes strings.
2. **Error locations.** Uncaught errors have no stack trace yet.
3. **Property attributes, `Proxy`/`Reflect`, decorators.**
4. **`.d.ts` files.** npm packages ship JS plus declarations. JS already
   runs (untyped); types from `.d.ts` files would give typed boundaries, and
   with them escape analysis across package boundaries.

## Layout

| path | contents |
|---|---|
| `src/lexer.rs`, `src/parser.rs`, `src/ast.rs` | front end |
| `src/module.rs` | module loading and Node-style resolution; export tables |
| `src/types.rs` | types and storage kinds |
| `src/check/` | checker: declarations, classes and type-level evaluation (`mod.rs`), expressions and calls (`expr.rs`), statements and narrowing (`stmt.rs`), modules (`modules.rs`), built-in signatures (`builtins.rs`) |
| `src/hir.rs` | typed IR |
| `src/codegen.rs` | slot allocation, lowering, inlining, peephole |
| `src/bytecode.rs` | the instruction set: `Instruction`, slots, call sites |
| `src/disasm.rs` | `--dump`: instructions with slots shown by variable name |
| `src/vm.rs` | the interpreter |
| `src/vm_task.rs` | generators, async functions, promises, the event loop, iteration |
| `src/escape.rs` | escape analysis: which allocations are scope-bound, and result regions |
| `src/runtime/heap.rs` | `Gc<T>`, the box header, `Trace`, the counting allocator, `--mem-stats` |
| `src/runtime/arena.rs` | scope arenas (bump allocation on the `Allocator` trait) |
| `src/runtime/cycles.rs` | the cycle collector (Bacon–Rajan trial deletion) |
| `src/runtime/gc.rs` | the tracing mark-sweep GC (`--features gc`), the baseline |
| `src/runtime/trace.rs` | `Trace` for every heap type |
| `src/runtime/` | values, objects, shapes, `inspect`, natives, BigInt arithmetic (`bigint.rs`), regular expressions (`regexp.rs`), coroutine and promise types (`task.rs`) |
| `src/prelude.ts`, `src/prelude_date.ts` | built-in classes written in TypeScript (`Error` family; `Date`, compiled only when used) |
| `src/node/` | Node built-in modules in TypeScript, and their natives (`natives.rs`) |
| `tests/programs/` | programs whose output must match Node |
| `tests/rts/` | rts-specific behaviour (checked `any` boundaries) |
| `bench/` | benchmarks and runners (`modes.py`: memory modes) |
| `bench/awfy/` | the Are-We-Fast-Yet suite: upstream JavaScript (`js/`) and typed ports (`ts/`) |
