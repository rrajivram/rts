# rts design

rts runs TypeScript without a garbage collector. It uses static types to decide,
at compile time, where each value is stored and which instruction operates on it.
This is a proof of concept. The long-term goal is to run arbitrary npm packages.

## Pipeline

```
source ──lexer──▶ tokens ──parser──▶ AST ──checker──▶ typed HIR ──codegen──▶ bytecode ──▶ VM
```

| module | job |
|---|---|
| `lexer` | Tokens with byte spans. Template literals are lexed as one token holding the parts. |
| `parser` | Hand-written recursive descent with Pratt expression parsing. Arrow functions and `<T>(…)` generic calls are found by speculative parsing (save the token position and restore it on failure). |
| `ast` | The untyped syntax tree. Type annotations are kept as `TypeExpr`. |
| `types` | Semantic types (`Type`), assignability, and the mapping from a type to a storage **kind**. |
| `check` | Resolves names, infers and checks types, narrows unions, and finds captured variables. Its output is HIR: every expression has a type, every identifier is resolved to a `VarId` or a global, and every implicit conversion (boxing, unboxing, number→string) is an explicit node. |
| `codegen` | Allocates typed registers and lowers HIR to bytecode. |
| `vm` | The interpreter loop, frames, and calls. |
| `runtime` | Values, objects, arrays, strings, and built-ins (`console`, `Math`, string and array methods). |

## Hybrid typing (option "c")

Every static type maps to one of two storage **kinds**:

| kind | holds | register file |
|---|---|---|
| `S` (scalar) | `number` (f64 bits), `boolean` (0/1) | `Vec<u64>`, untagged, never refcounted |
| `V` (value) | everything else: `string`, objects, arrays, functions, unions, `any`, `unknown`, generics | `Vec<Value>`, tagged, refcounted |

A frame has a window into each file. When both operands are known to be numbers,
an instruction such as `AddN d, a, b` is a plain f64 add on two `u64` slots, with
no tag check and no refcount traffic.

Values whose static type is not precise (`any`, `unknown`, unions, type
parameters) are stored as boxed `Value`s, and operations on them check the tag at
run time. The checker inserts the conversion at each boundary:

- `Box{N,B}` turns a scalar into a value. This always succeeds.
- `Unbox{N,B}` turns a value into a scalar. It checks the tag and throws a
  `TypeError` on a mismatch, so a lie in a `.d.ts` or an `as` cast cannot corrupt
  memory.

### Unsound TypeScript

TypeScript's unsound spots (`any`, `as`, array covariance, structural aliasing)
are handled with run-time checks on slow paths, never by trusting the type:

- **Arrays** are specialized by storage: `ArrStore::Num(Vec<f64>)` or
  `ArrStore::Val(Vec<Value>)`. A `number[]` starts as `Num`. A typed read such as
  `GetIdxN` takes the fast path when the storage is `Num`. If something wrote a
  non-number through a covariant alias, the storage has degraded to `Val`, and the
  read checks the element instead.
- **Object fields** are `Value`s at a slot index. Typed reads check the tag with a
  cheap, predictable branch.

### Generics

Generics are erased: type parameters are of kind `V`, and callers box and unbox
around them. Monomorphization can come later.

## Objects without hash lookups

- A **shape** is an ordered list of property names. Objects keep their fields in a
  `Vec<Value>` indexed by the shape.
- **Class instances** have a shape fixed by the class. A subclass extends its
  parent's shape as a prefix. When the receiver's static type is a class, a field
  access compiles to a constant slot index (`GetField`), and a method call indexes
  the class vtable (`CallMethod`).
- **Structural types** (interfaces, object literal types) and `any` use
  `GetProp` with a per-instruction **inline cache** of (shape pointer → slot).
  This is necessary because structurally typed objects can have any layout.

## Variables and calls

- Locals are resolved to register slots at compile time, so there are no scope
  objects or name lookups at run time.
- A local captured by a closure is detected by the checker and stored in a cell
  (`Rc<RefCell<Value>>`). Closures hold their captured cells in a `Vec`.
- Calls pass arguments in the callee's register windows. A function value
  carries its signature as a list of parameter kinds and a return kind. A call
  site states the kinds it is passing:
  - If they match the callee's (the common case), arguments are copied straight
    across.
  - Otherwise, each argument is boxed or unboxed. This handles dynamic calls
    through `any`, generic callbacks, and function-type subtyping uniformly.
- A call to a top-level function that is never reassigned compiles to
  `CallDirect`, with no closure lookup.

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

## Supported subset (POC)

**Types:** `number`, `boolean`, `string`, `void`, `null`, `undefined`, `any`,
`unknown`, `never`, literal types, `T[]` and `Array<T>`, tuples (as arrays),
function types, object type literals, interfaces (including `extends`), type
aliases, unions with narrowing, classes, numeric enums, and generic functions
and classes (erased).

**Statements:** `let`/`const`/`var`, `if`, `while`, `do`, `for`, `for-of`, `for-in`,
`break`/`continue` (with labels), `switch`, `return`, `throw`/`try`/`catch`,
functions, classes (fields, constructors, methods, `extends`, `super`, parameter
properties, static methods), `interface`, `type`, `enum`.

**Expressions:** the usual operators, template literals, optional chaining `?.`,
`??`, arrows and closures, `new`, object and array literals, spread, destructuring,
getters and setters, `as`, `!`, `typeof`, and `instanceof`.

**Narrowing:** `typeof x === "…"`, `x === null` / `undefined`, truthiness,
`instanceof`, and discriminant property checks. Narrowing applies in branches
and after an `if` whose branch always exits.

**Not yet supported:** async/await, generators, modules, decorators, regular
expressions, symbols and user-defined iterators. See README "Next steps".
