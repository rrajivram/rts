# rts

rts is a TypeScript runtime written in Rust. It has **no garbage collector**
and **uses static types to choose storage and instructions ahead of time**.

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
```

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
- Prototype chains on plain objects (`Object.create(proto)` copies),
  `new` on a class value of unknown type.
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

Roughly in order of what's needed to run real npm packages:

1. **`Buffer`, `TextEncoder`, `URL`, `fetch`/`http`.** Most packages that do
   I/O need at least `Buffer`. `node:fs` currently reads and writes strings.
2. **Error locations.** Uncaught errors have no stack trace yet. A pc → source
   line table is reserved (`FuncCode::lines`); `Error.stack` should be
   filled from it.
3. **Reference cycles.** In order:
   1. Static analysis of the class field graph, since most types cannot form
      cycles.
   2. Inferred `weak` back-references (parent pointers).
   3. A trial-deletion cycle collector limited to cycle-capable types.
4. **Prototype chains and property attributes** for plain objects
   (`Object.create(proto)`, `defineProperty` with `enumerable`/`writable`),
   `Proxy`/`Reflect`, decorators.
5. **Unboxed object fields.** Store `number` fields as `f64` in the object
   layout. Today `ReadFieldNumber` pays a `RefCell` borrow and a tag check, which is
   the main cost in `nbody`.
6. **Faster calls.** Calls cost about 30ns. To reduce that: inline small
   non-recursive functions (the HOF inliner already does this for callbacks),
   use a slimmer frame layout, and use direct-threaded dispatch.
7. **`.d.ts` files.** npm packages ship JS plus declarations. JS already
   runs (untyped); types from `.d.ts` files would give typed boundaries.

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
| `src/runtime/` | values, objects, shapes, `inspect`, natives, BigInt arithmetic (`bigint.rs`), regular expressions (`regexp.rs`), coroutine and promise types (`task.rs`) |
| `src/prelude.ts`, `src/prelude_date.ts` | built-in classes written in TypeScript (`Error` family; `Date`, compiled only when used) |
| `src/node/` | Node built-in modules in TypeScript, and their natives (`natives.rs`) |
| `tests/programs/` | programs whose output must match Node |
| `tests/rts/` | rts-specific behaviour (checked `any` boundaries) |
| `bench/` | benchmarks and runner |
