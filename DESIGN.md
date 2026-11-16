# rts design

rts runs TypeScript without a garbage collector. It uses static types to decide,
at compile time, where each value is stored and which instruction operates on it.
This is a proof of concept. The long-term goal is to run arbitrary npm packages.

## Pipeline

```
files ──loader──▶ modules (ASTs) ──checker──▶ typed HIR ──codegen──▶ instructions ──▶ VM + event loop
```

| module | job |
|---|---|
| `module` | Loads the entry file and everything it imports or `require`s, resolving specifiers as Node does, and orders modules for evaluation. All files share one offset space for spans (`error::Sources`), so an error names its file. |
| `lexer` | Tokens with byte spans. Template literals are lexed as one token holding the parts (cooked and raw). A `/` where an expression may start begins a regular expression. |
| `parser` | Hand-written recursive descent with Pratt expression parsing. Arrow functions and `<T>(…)` generic calls are found by speculative parsing (save the token position and restore it on failure). |
| `ast` | The untyped syntax tree. Type annotations are kept as `TypeExpr`. |
| `types` | Semantic types (`Type`), assignability, and the mapping from a type to a storage **kind**. |
| `check` | Resolves names, infers and checks types, narrows unions, and finds captured variables. Its output is HIR: every expression has a type, every identifier is resolved to a `VarId` or a global, and every implicit conversion (boxing, unboxing, number→string) is an explicit node. |
| `codegen` | Gives every variable and temporary a typed slot and lowers HIR to instructions. |
| `bytecode` | The instruction set (`Instruction`), the slot types, and call sites. |
| `disasm` | `rts --dump`: prints instructions with slots shown by variable name. |
| `vm` | The interpreter loop, frames, and calls. |
| `runtime` | Values, objects, arrays, strings, and built-ins (`console`, `Math`, string and array methods). |

## Instructions and slots

The instruction style follows rjs: each instruction is a named variant with
named fields, such as `Arithmetic { op, left, right, output }` or
`JumpIf { when, condition, to }`. The operands are **slots**. Every local
variable, parameter and temporary of a function has a numbered slot in its
frame, resolved at compile time, so nothing is looked up by name at run time.

Each slot has a type fixed by the compiler, so every instruction knows what
its operands hold:

| slot type | holds | stored in |
|---|---|---|
| `NumSlot` | a `number` | the frame's scalar slots (`Vec<u64>`): raw f64 bits, untagged, never refcounted |
| `BoolSlot` | a `boolean` | the frame's scalar slots: 0 or 1 |
| `BigSlot` | a `bigint` | two consecutive scalar slots: an `i128`, low half first |
| `ValSlot` | everything else: `string`, objects, arrays, functions, unions, `any`, `unknown`, generics | the frame's value slots (`Vec<Value>`): tagged, refcounted |

So `NumSlot(3)` and `ValSlot(3)` are different places. When both operands are
known numbers, `Arithmetic { op: Add, left, right, output }` is a plain f64
add, with no tag check and no refcount traffic.

BigInts follow rjs and are `i128`s rather than arbitrary precision; a result
that doesn't fit is a `RangeError`. Because one fits in two scalar slots, a
typed `bigint` is as unboxed as a `number`: `BigIntArithmetic`,
`CompareBigInts`, `ReturnBigInt` and bigint call arguments copy 16 bytes and
never allocate. Only BigInts stored as values (`any`, arrays, fields,
closures) are boxed, as `Value::BigInt(Rc<i128>)`. Untyped arithmetic on two
`any` operands produces a value (`DynamicArithmeticToValue`), since it may
turn out to be a BigInt.

`rts --dump` shows slots by the variable they hold (`n`, `total`). It shows a
slot preloaded with the constant 3 as `#3`, and temporaries as `num4`,
`bool0`, `big6` or `val2`:

```
function fib(n: number)
  ; slots: 6 number/boolean, 1 value
    LoadNumber { value: 2, output: num2 }
    JumpIfCompare { op: Lt, when: false, left: n, right: num2, to: L4 }
    CopyNumber { from: n, to: num1 }
    Jump { to: L9 }
L4:
    AddConstant { left: n, constant: -1, output: num3 }
    Call { function: fib, args: [num3], output: num2 }
    AddConstant { left: n, constant: -2, output: num5 }
    Call { function: fib, args: [num5], output: num4 }
    Arithmetic { op: Add, left: num2, right: num4, output: num1 }
L9:
    CopyNumber { from: num1, to: num2 }
    ReturnNumber { value: num2 }
```

Where rjs differs: rjs looks variables up by name in scope objects
(`Var("n")`), with registers `AX`–`DX` and a push/pop stack for temporaries.
rts resolves every name to a slot before running, which is most of why it is
faster (see the README benchmarks).

## Hybrid typing (option "c")

Values whose static type is not precise (`any`, `unknown`, unions, type
parameters) are stored as boxed `Value`s, and operations on them check the tag at
run time. The checker inserts the conversion at each boundary:

- `BoxNumber` / `BoxBool` copy a number or boolean slot into a value slot.
  This always succeeds.
- `UnboxNumber` / `UnboxBool` copy a value slot into a number or boolean slot.
  They check the tag and throw a `TypeError` on a mismatch, so a lie in a
  `.d.ts` or an `as` cast cannot corrupt memory.

### Unsound TypeScript

TypeScript's unsound spots (`any`, `as`, array covariance, structural aliasing)
are handled with run-time checks on slow paths, never by trusting the type:

- **Arrays** are specialized by storage: `ArrStore::Num(Vec<f64>)` or
  `ArrStore::Val(Vec<Value>)`. A `number[]` starts as `Num`. A typed read such as
  `ReadElementNumber` takes the fast path when the storage is `Num`. If something wrote a
  non-number through a covariant alias, the storage has degraded to `Val`, and the
  read checks the element instead.
- **Object fields** are `Value`s at a slot index. Typed reads check the tag with a
  cheap, predictable branch.

### Generics

Generics are erased: type parameters live in value slots, and callers box and unbox
around them. Monomorphization can come later.

## Objects without hash lookups

- A **shape** is an ordered list of property names. Objects keep their fields in a
  `Vec<Value>` indexed by the shape.
- **Class instances** have a shape fixed by the class. A subclass extends its
  parent's shape as a prefix. When the receiver's static type is a class, a field
  access compiles to a fixed field position (`ReadField`), and a method call
  indexes the class vtable (`Call` with a `CallTarget::Method`).
- **Structural types** (interfaces, object literal types) and `any` use
  `ReadProperty` with a per-instruction **inline cache** of (shape pointer → field position).
  This is necessary because structurally typed objects can have any layout.

## Variables and calls

- Locals are resolved to slots at compile time, so there are no scope
  objects or name lookups at run time.
- A local captured by a closure is detected by the checker and stored in a cell
  (`Rc<RefCell<Value>>`). Closures hold their captured cells in a `Vec`.
- Calls copy arguments into the callee's parameter slots. A function value
  carries its signature as a list of parameter kinds and a return kind. A call
  site states the kinds it is passing:
  - If they match the callee's (the common case), arguments are copied straight
    across.
  - Otherwise, each argument is boxed or unboxed. This handles dynamic calls
    through `any`, generic callbacks, and function-type subtyping uniformly.
- A call to a top-level function that is never reassigned compiles to
  a `Call` with `CallTarget::Direct`, with no closure lookup.

## Memory: scopes first, counts second, no tracing GC

The thesis rts tests: memory managed by scope (and, where scope can't be
proven, by reference counting) is better than a tracing garbage collector.
Three mechanisms, from most to least static:

1. **Scope arenas** (`src/escape.rs`, `src/runtime/arena.rs`). A whole-program
   escape analysis finds allocations that can never outlive a block. Those are
   bump-allocated in the block's arena and freed together when the block
   exits. Their reference counts are never what frees them.
2. **Reference counting** (`src/runtime/heap.rs`). Every other heap value is a
   `Gc<T>` with a count, freed the moment the count reaches zero.
3. **Cycle collection** (`src/runtime/cycles.rs`). Reference counting can't
   free garbage cycles. Boxes that might head one are buffered, and a
   trial-deletion collector (Bacon & Rajan 2001) checks them at safepoints.

A tracing mark-sweep GC (`src/runtime/gc.rs`, `cargo build --features gc`)
exists only as the baseline for the comparison.

Numbers and booleans in typed code never touch the heap at all: they live
unboxed in number slots.

### The heap pointer

`Gc<T>` is rts's own pointer, not `std::rc::Rc`. On stable Rust, `Rc` can't
take a custom allocator (`Rc::new_in` is unstable), but the `Allocator` trait,
`Box::new_in` and `Vec::new_in` are stable. Every box is a 16-byte header plus
the value:

| field | |
|---|---|
| `rc: u32` | the count (arena boxes count from `ARENA_COUNT`, 2³⁰) |
| `flags: u32` | acyclic, buffered, arena, dead, two colour bits, mark bit |
| `vt: &VTable` | per-type `trace`, `drop` and `Layout`, for type-erased boxes |

Boxes are allocated through `Allocator`: `Global` for the heap, an `Arena` for
scope-bound objects. The process allocator is a counting wrapper around
`System`, so `--mem-stats` reports exact live and peak heap bytes. The slot
stacks are reserved straight from the system allocator: they are address
space, not heap.

`Trace` (`src/runtime/trace.rs`) lists every `Gc` pointer a value holds. Both
collectors use it. One subtlety: containers shared through a plain `Rc`, such
as a `Shape` holding a prototype, hold one counted reference however many
boxes share them. The cycle collector must not see their pointers (it would
subtract the reference once per sharer); the tracing GC must.

### Escape analysis

The analysis runs on the HIR after type checking. It is flow-insensitive
points-to over allocation sites, one function at a time:

- **Points-to sets.** Each local variable gets the set of sites it may hold.
  Each site gets the set of sites stored into it (its *contents*,
  field-insensitive).
- **What makes a site escape.** Being stored in a global, a captured variable
  or an unknown object, or being passed to an unknown callee, thrown or
  yielded. A value read out of an object may also be something the analysis
  never saw, so storing into such a value counts as an escape.
- **Calls.** A call to a known function uses that function's *summary*: for
  each parameter, whether it escapes, whether it may come back as the result,
  and which other parameters' objects it is stored into. Summaries are
  computed to a whole-program fixpoint; if it doesn't settle, every summary
  becomes "everything escapes". A virtual call joins the summaries of all
  overrides. Built-ins come from a table, and anything not in it makes its
  arguments escape.
- **Region choice.** A site that doesn't escape is bound to the innermost
  block enclosing every variable and container it can reach. It stays on the
  heap if a loop body lies between the allocation and that block, because
  the arena would otherwise grow with each iteration.
- **Result regions** (region polymorphism). Suppose every value a function
  returns is one of its own fresh allocations, as with `plus()`, `minus()`
  or `times()`. Then each call to it is treated as an allocation site in the
  caller, and the caller picks the region. At run time the call passes a
  region index (`CallSite.region`), and the callee's returned objects are
  allocated there (`New*In { up: RESULT_REGION }`). Without a passed region
  they go to the heap. `return f(...)` passes the caller's region on.

Codegen gives each chosen block `EnterRegion` / `ExitRegion`. The exit first
clears every slot the block used (its variables and temporaries) and then
resets the arena. `break`, `continue`, `catch` and an inlined `return` leave
regions with `ExitRegionsTo`, and a frame's regions die with the frame.

**Checked, not trusted.** Each arena box counts from `ARENA_COUNT + 1`. At
reset, every value is dropped, newest first and repeated until nothing
changes, and each box's count must come back to `ARENA_COUNT`. A box that is
still referenced indicates an analysis bug. Its memory is then deliberately
leaked rather than freed, and counted (`outlived their scope` in
`--mem-stats`; `RTS_ARENA_CHECK=1` reports each case). So a wrong analysis
can cost memory but never cause a use-after-free. Every test and benchmark
reports 0.

### Cycle collection

A decrement that leaves a count above zero buffers the box as a possible
cycle root, unless its type can't form a cycle. That covers leaf types
(strings, BigInts, regexes, symbols) and instances of classes whose fields
are all primitive. The checker computes the class property (`HClass::acyclic`);
a dynamic write of an object into such an instance clears the flag.

When the buffer fills, the collector first purges dead and re-incremented
entries, which is cheap. It runs the three trial-deletion phases (mark gray,
scan, collect white) only if enough live candidates remain. Before freeing
the white boxes, it restores the counts that trial deletion subtracted from
live children. Otherwise freeing would subtract them a second time.

Collection runs only at safepoints: right after a frame is pushed, when no
heap value is mutably borrowed. A `RefCell` that is borrowed anyway is treated
as opaque, which is conservative: its children just look externally referenced.

### Tracing GC (baseline)

With `--features gc`, `clone` and `drop` of a `Gc` do nothing and every box
is on an intrusive list. When live bytes reach twice what survived the last
collection (8 MB minimum), the next safepoint marks from the roots and sweeps
the list.

- **Roots:** the active slot stacks, globals, frame closures, running
  coroutines, the event loop's jobs and timers, and constants.
- **Native code:** a native that calls back into the VM may hold values in
  Rust locals, so collection waits until such calls return (`gc_block`).
- **Event-loop jobs:** each job roots what it holds and then runs its
  callbacks with collection allowed.
- **Stress settings:** `RTS_GC_MIN_HEAP=0` collects at every safepoint, and
  `RTS_CC_THRESHOLD=1` does the same for the cycle collector. The tests pass
  under both.

## Modules

`module.rs` loads every module the entry file reaches through `import`,
`export … from`, `require("…")` and `import("…")` with a literal
specifier. Specifiers resolve like Node: relative files (with TypeScript's
`./x.js` → `./x.ts` mapping), directories (`index.*`, `package.json`
`main`), and bare names through `node_modules` (`exports` with
conditions and `./*` patterns, then `module`/`main`). `node:` names and
bare built-in names load the TypeScript sources in `src/node/`.

The checker is whole-program. Each module is a scope under the global scope
(built-ins and the prelude). `check/modules.rs` hoists the declarations of
*every* module first (types, then values), then binds each import by copying
the exporter's binding (the same `VarId`, class or interface) into the
importing scope, then checks module bodies in evaluation order (depth-first
post-order, as Node does) as parts of the one top-level function. So an
import costs nothing at run time, it is live (a re-assigned `let` export is
seen by importers), cycles work for anything used after both modules have run,
and types flow across files unchanged. `ns.x` for a namespace import is
rewritten to the binding of `x`; `ns` used as a value becomes an object of
the exports.

CommonJS files (`.cjs`, or `.js` without `import`/`export`) are wrapped:
`module`, `exports`, `__filename` and `__dirname` are declared before the
body and `module.exports` is captured after it; that value is the module's
default export, and `require` of the module evaluates to it. JavaScript
files are checked with every binding `any` (per function and class, so a
typed module calling into JavaScript stays typed).

## Generators, async functions and the event loop

A generator or async function runs in an ordinary frame with ordinary
instructions; four instructions suspend it:

- `CreateGenerator` (first instruction of a generator body, after parameter
  setup) moves the frame's slots and `try` handlers into a new `Coroutine`,
  pops the frame and returns the generator object.
- `Yield { value, received, mode }` does the same and hands `value` to the
  resumer. Resuming (`next`/`throw`/`return`, or a `for-of` step) pushes a
  frame on top of the stack, moves the slots back, writes the resumption
  value into `received` and its kind into `mode`, and runs the interpreter
  until the next suspension. Codegen follows each `Yield` with
  `mode == 1 → throw`, `mode == 2 → return` (through `finally` blocks).
- `StartAsync` gives an async call a promise; the frame keeps running.
- `Await { value, received, mode }` suspends until a promise settles. The
  first `await` of a call returns the promise to the caller.

Typed slots are copied as raw words, so a suspended generator's `number`
loop counter stays an unboxed `f64`. Coroutine frames are recorded in a side
stack (`Vm::co_frames`) rather than in `Frame`, so ordinary calls don't pay
for them.

Promises (`runtime/task.rs`, `vm_task.rs`) implement the ECMAScript job
model: settling a promise queues a microtask per reaction, resolving with a
thenable queues a `ResolveThenable` job, and `await` uses `PromiseResolve`
plus one reaction. That is what makes callback order identical to Node.
After the top-level code, the event loop drains `process.nextTick`
callbacks and microtasks, then runs due timers (each followed by a
microtask drain) and immediates, sleeping until the next timer. A promise
rejected with no handler when the microtask queue empties ends the program,
as in Node.

The iterator protocol (`GetIterator`, `IteratorStep`, `IteratorClose`,
`IteratorResume` for `yield*`) has native fast paths for generators and
built-in iterators; anything else calls `[Symbol.iterator]()` and `next()`.
A `for-of` over an iterator is wrapped in a handler so that `break`,
`return` and exceptions call the iterator's `return()`. Loops over values
statically known to be arrays, strings, maps or sets keep the indexed fast
path. These rarely hot instructions are executed out of line
(`Vm::exec_extended`), which keeps the main interpreter loop small.

## Symbols, regular expressions, and the prelude

A symbol-keyed property is stored under a key string no program can spell
(`"\u{2}Symbol.iterator"`), so objects and inline caches need nothing
new; key enumeration skips them and `inspect` shows them as Node does.
Regular expressions are translated from JavaScript syntax to `fancy-regex`
(backreferences, lookaround, named groups; ASCII `\d`/`\w`), with UTF-16
indices at the boundary.

Built-in classes that are easier to write in TypeScript than in Rust live in
`src/prelude.ts` (the `Error` family) and `src/prelude_date.ts` (`Date`,
compiled only into programs that mention it), and Node's modules in
`src/node/*.ts`, calling natives named `__rts_*`.

## Slot stacks

The scalar and value slot stacks are reserved once (32 MB of address space
each, zeroed lazily by the OS, so only used pages cost memory) and never
reallocated. A running frame's slot pointers therefore stay valid across
calls, getters and resumed generators; a frame that does not fit is a
`RangeError` ("Maximum call stack size exceeded").

## Supported language

See the README ("Supported TypeScript and Node") for the full list. In
brief: modules (ES and CommonJS), generators and async functions, classes
and generics, mapped/conditional/indexed types, regular expressions,
symbols and iterators, and the common Node globals and built-in modules.

**Not yet supported:** decorators, `Proxy`/`Reflect`, prototype chains on
plain objects, `Buffer` and the networking modules. See README.
