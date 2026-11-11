# rts design

rts runs TypeScript without a garbage collector. It uses static types to decide,
at compile time, where each value is stored and which instruction operates on it.
This is a proof of concept. The long-term goal is to run arbitrary npm packages.

## Pipeline

```
source ──lexer──▶ tokens ──parser──▶ AST ──checker──▶ typed HIR ──codegen──▶ instructions ──▶ VM
```

| module | job |
|---|---|
| `lexer` | Tokens with byte spans. Template literals are lexed as one token holding the parts. |
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

## Supported subset (POC)

**Types:** `number`, `bigint` (128-bit), `boolean`, `string`, `void`, `null`, `undefined`, `any`,
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
