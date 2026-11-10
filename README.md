# rts

rts is a TypeScript runtime written in Rust. It has **no garbage collector**
and **uses static types to choose storage and instructions ahead of time**.

This is a proof of concept. It runs a useful subset of TypeScript directly from
source, with no transpile step. The long-term goal is to run arbitrary npm
packages.

```sh
cargo build --release
./target/release/rts program.ts           # run
./target/release/rts --dump program.ts    # show the typed bytecode
tests/run.sh                              # conformance tests (compared against Node)
bench/run.py                              # benchmarks vs node, node --jitless, rjs
```

## How it works

```
lexer → parser → type checker → typed HIR → codegen → typed register bytecode → VM
```

See [DESIGN.md](DESIGN.md) for details. In short:

- **Two register files.** `number` and `boolean` live unboxed in a `u64` file
  and never touch the heap or a refcount. Everything else is a 16-byte tagged
  `Value`. When the types are known, `a + b` compiles to `AddN`, a raw f64 add
  with no tag check.
- **Hybrid typing.** `any`, `unknown`, unions and generics fall back to boxed
  values with dynamic operations. At each boundary the checker inserts a
  *checked* unbox, so an incorrect `as`, a bad `.d.ts` or a covariant array
  write raises a `TypeError` instead of corrupting memory (see `tests/rts/`).
- **No GC.** Heap values are `Rc`, freed deterministically the moment they
  die. Frames release their references when they return, and blocks release
  theirs when they exit. Reference cycles leak (see Next steps).
- **Objects.** Class instances have fixed layouts, so typed field access is a
  constant slot index (`GetFieldN`). Method calls go through a vtable.
  Structural types and `any` use shape-keyed inline caches.
- **Arrays.** `number[]` uses `Vec<f64>` storage. It degrades to `Vec<Value>`
  if something writes a non-number into it through `any`.

### Optimizations that rely on knowing types

- Every variable is resolved to a register slot at compile time. Top-level
  variables used only by top-level code live in registers; the rest live in
  typed global slots.
- Numeric constants are kept in registers inside loops, and `const x = 5` reads
  are folded to the literal.
- Fused compare-and-branch (`JCmpN`), rotated loops (one branch per
  iteration), `AddNK` for `x + k`, an integer fast path for `%`, and a
  peephole pass that removes jumps to the next instruction.
- `arr.map/filter/forEach/reduce/some/every/find/findIndex` compile to
  bytecode loops. Arrow callbacks are **inlined** into the loop, so
  `xs.map(x => x * 2)` on a `number[]` runs about 4 instructions per element,
  with no call and no boxing.
- Hot built-ins (`Math.*`, `push` onto `number[]`, `charCodeAt`) are dedicated
  instructions.
- Call sites state the kinds of the values they pass. When those match the
  callee's parameter kinds (the usual case), arguments are copied from
  register to register. Otherwise the VM converts per argument, which is how
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

rts is faster than `node --jitless` on everything here, and on several
benchmarks it is within startup time of JIT Node. Comparing the two `complex`
rows shows what type annotations are worth: about 2× on the same program.

**Memory (alloc_churn, peak RSS):** rts **3.3 MB** vs Node **54 MB**
(jitless: 48 MB). Without a collector, garbage never accumulates.

## Supported TypeScript (POC subset)

- **Types:** primitives, literal types, arrays, tuples, functions, object
  types, interfaces (including recursive, generic, `extends`, merging), type
  aliases, unions, intersections of object types, enums (numeric and string),
  classes and generics, `keyof`, `typeof`, `Record`, `Partial`, `Readonly`,
  `Map<K,V>`, `Set<T>`.
- **Narrowing:** `typeof`, `===`/`!==` with `null`, `undefined` or literals,
  truthiness, `instanceof`, discriminant properties (`if`/`switch`),
  `Array.isArray`, `in`, and narrowing after early `return`/`throw`.
- **Classes:** fields, parameter properties, constructors, methods, getters
  and setters, `extends`/`super`, abstract classes, static members, virtual
  dispatch.
- **Functions:** closures with per-iteration loop bindings, arrows, defaults,
  optional and rest parameters, destructuring parameters, named function
  expressions, generic inference (including from callbacks).
- **Statements:** all loops, labels, `switch`, `try`/`catch`/`finally`
  (including `return` through `finally`), destructuring (array/object,
  defaults, rest), spread, optional chaining, `??`, template literals.
- **Built-ins:** `console`, `Math`, `JSON`, `Object.keys/values/entries/assign`,
  `Number.*`, `parseInt`/`parseFloat`, `String`, most string and array
  methods, `Map`, `Set`, and `Error`/`TypeError`/`RangeError` (written in
  TypeScript in `src/prelude.ts`).
- `console.log` formatting follows Node's `util.inspect` (line breaking and
  array grouping), so test output can be compared with Node byte for byte.

## Next steps

Roughly in order of what's needed to run real npm packages:

1. **Modules.** `import`/`export` across files, Node resolution, and
   `package.json` `exports`. The checker is whole-program, so each module can
   be a scope within one program.
2. **Generators and async/await.** Frames already live on explicit VM stacks
   and calls don't recurse in Rust, so a suspended frame can be moved into a
   heap object and resumed later. Promises and an event-loop microtask queue
   build on that.
3. **Reference cycles.** In order:
   1. Static analysis of the class field graph, since most types cannot form
      cycles.
   2. Inferred `weak` back-references (parent pointers).
   3. A trial-deletion cycle collector limited to cycle-capable types.
4. **Unboxed object fields.** Store `number` fields as `f64` in the object
   layout. Today `GetFieldN` pays a `RefCell` borrow and a tag check, which is
   the main cost in `nbody`.
5. **Faster calls.** Calls cost about 30ns. To reduce that: inline small
   non-recursive functions (the HOF inliner already does this for callbacks),
   use a slimmer frame layout, and use direct-threaded dispatch.
6. **`.d.ts` and JavaScript files.** npm packages ship JS plus declarations.
   Plain JS is all `any`; it already runs on the dynamic paths, but slowly.
   Types from `.d.ts` files would give typed boundaries.
7. **Missing language features:** regular expressions, symbols and
   iterators (`for-of` over user iterables), class expressions, computed keys,
   object-literal getters, `BigInt`, `Date`, mapped and conditional types,
   static blocks, and labeled `continue` in inlined callbacks.
8. **Error locations.** Uncaught errors have no stack trace yet. A pc → source
   line table is reserved (`FuncCode::lines`).

## Layout

| path | contents |
|---|---|
| `src/lexer.rs`, `src/parser.rs`, `src/ast.rs` | front end |
| `src/types.rs` | types and storage kinds |
| `src/check/` | checker: declarations and classes (`mod.rs`), expressions and calls (`expr.rs`), statements and narrowing (`stmt.rs`), built-in signatures (`builtins.rs`) |
| `src/hir.rs` | typed IR |
| `src/codegen.rs` | register allocation, lowering, inlining, peephole |
| `src/bytecode.rs`, `src/vm.rs` | instruction set and interpreter |
| `src/runtime/` | values, objects, shapes, `inspect`, natives |
| `tests/programs/` | programs whose output must match Node |
| `tests/rts/` | rts-specific behaviour (checked `any` boundaries) |
| `bench/` | benchmarks and runner |
