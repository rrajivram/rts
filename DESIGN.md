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

## Memory: no tracing GC

- Heap values (strings, arrays, objects, closures) are `Rc`. They are freed
  deterministically when the last reference goes away.
- Numbers and booleans in typed code never touch the heap or a refcount.
- **Known gap:** reference cycles leak. Examples are a doubly linked list, a
  parent pointer, or a closure stored in a variable it captures. Planned
  mitigations, in order:
  1. Static cycle analysis over the class field graph. Most types provably cannot
     form cycles and need nothing.
  2. Weak back-references, inferred or annotated.
  3. A trial-deletion cycle collector (Bacon–Rajan) restricted to
     cycle-capable types.

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
