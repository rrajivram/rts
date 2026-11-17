# Code base review: simplifications and performance

Reviewed at commit `acadfff` (32,900 lines of Rust in `src/`). No code was
changed for this review. Every number below was measured on this machine
(Apple silicon, macOS) with the release build, using existing flags, the
macOS `sample` profiler (1 ms interval) and throwaway scripts outside the
repository.

**How much was read.** Read in full: the runtime core (`runtime/mod.rs`,
`heap.rs`, `cycles.rs`, `gc.rs`, `arena.rs`, `trace.rs`, `natives.rs`), the
interpreter (`vm.rs`, `vm_task.rs`), `bytecode.rs`, `lib.rs`, `main.rs`, the
JIT driver (`jit::compile`), and about half of `codegen.rs` (set-up, blocks,
expressions, calls, inlining). Scanned with targeted searches and
measurements, not line by line: the checker (`check/`, 6,700 lines), parser,
lexer, `escape.rs`, `module.rs`, `node/`, the rest of `jit/`. Findings in
the scanned parts are therefore less complete; the front end as a whole is
not a bottleneck (see P3).

## Summary

Ranked by expected gain. "Share" is the part of run time the item touches,
not a promised speed-up.

| # | Recommendation | Evidence | Effort |
|---|----------------|----------|--------|
| P1 | Calls between compiled functions should not go through the interpreter (virtual, closure and by-name calls) | Call bridge is 42% of DeltaBlue samples, 31% of Richards, 27% of CD, 13% of Havlak; only 11% of DeltaBlue samples are in compiled code | Large |
| P2 | `this.field(...)` on a class instance: read the field by position, don't look it up by name | Richards: 2 string-hash lookups per call, 20% of samples | Small |
| P3 | Start-up compilation: turn off the Cranelift verifier, import only the functions each function calls, then compile on first use | 234 ms of a 252 ms start on the AWFY harness; cost grows quadratically (8,000 functions: 7.7 s) | Small, small, medium |
| P4 | Cycle collector cost | 10% of DeltaBlue, 21% of Havlak and CD run time | Medium |
| P5 | Strings: one-character string table, one allocation per string, `===` fast path | Json: 25% malloc/free, 12% `memcmp`, 9% `h_op`, 8% `strict_eq` | Small to medium |
| P6 | Built-ins should take `&[Value]`, not `Vec<Value>` | One heap allocation per built-in call; Json 7%, Storage 44% in malloc/free | Medium (mechanical) |
| P7 | Smaller objects: 64 bytes of header before the first field | Havlak 12.4 M objects; malloc/free 24% of samples | Medium |
| S1 | Stop keying side tables by HIR node address while codegen clones HIR | Scope arenas are silently lost in inlined callbacks and nested `new` arguments | Medium |
| S2–S4 | One method lookup, one "call function by id", codegen helper folding | Four copies of the class-chain walk, five synthetic closures | Small |
| S8 | Lints and tests: clippy fails, 1 test configuration of 11 is run, 3 failures under `--no-types` | Listed below | Small |

Suggested order: P2 and the two small parts of P3 (hours each, large visible
effect), then P1 (the largest gain, and it subsumes "faster polymorphic
calls" from the stage-3 list), then P5/P6, then P4 with P7.

---

## Performance

### P1. Compiled code calls compiled code through the interpreter

Compiled code calls another compiled function directly only when the target
is known at compile time (a direct call, or a method with one
implementation). Everything else, virtual methods with several
implementations, closures, by-name calls, goes:

`h_call` (C ABI, clones `this`) → `Vm::jit_call_*` → `call_native_values`
(writes every argument into a `u64` array) → `enter_native_code` (updates
the context, checks status) → the function's entry trampoline (unpacks the
array) → the function.

Top-of-stack samples (3 s at 1 ms, standard problem sizes, typed AWFY
ports, 1,700–2,100 samples each; only the 14 largest entries were
tallied, so shares are lower bounds):

| Benchmark | In compiled code | Call bridge (`h_call`, `call_native_values`, `call_slice_unblocked`, `jit_call_func`) | Rest, largest first |
|-----------|-----------------|-------------|-------|
| DeltaBlue | 11% | 42% | cycle collector 11%, malloc/free 10% |
| Richards | 49% | 31% | by-name lookup 20% (P2) |
| CD | 13% | 27% | cycle collector 21%, malloc/free 7% |
| Havlak | 5% | 13% | malloc/free 24%, cycle collector 14%, interpreter loop 5% |
| Json | 6% | 7% (plus `call_native` 5%) | strings 34%, malloc/free 25% (P5) |
| Storage | 14% | 6% (plus `call_native` 3%) | malloc/free 44%, `Vec<Value>` fill and drop 22% |
| Bounce, List, Towers, Permute | 94–100% | — | — |

Havlak spends 5% in the interpreter loop (`Vm::run`) although every
Havlak function is compiled. I did not trace which call it is; the likely
cause is a call that the compiled entry refuses (an argument or receiver
kind it doesn't take, or too few arguments) and that then runs
interpreted. There is no counter for these fallbacks; `RTS_JIT_DEBUG`
should report them per function.

Recommendations:

1. **Virtual calls**: give each run-time class a table of native code
   addresses beside `vtable: Vec<u32>`. When every implementation of a
   vtable slot is compiled with the same parameter and result kinds (known
   at compile time from `Elig::subclasses`), emit three loads and an
   indirect call with the native signature. Otherwise keep `h_call`.
2. **Closure calls**: store the native entry (typed signature) in the
   function table; when the callee's static type fixes the kinds, emit a
   guarded indirect call (`Closure::func` → entry, `caps` pointer as the
   hidden first argument), falling back to `h_call` when the function isn't
   compiled.
3. **The bridge itself** (still needed for interpreter → compiled calls):
   `call_slice_unblocked` clones the function `Rc`; `jit_call_func`
   allocates a `Closure` on its fallback path; `h_call` clones `this`
   although the callee borrows it.

### P2. A function stored in a field is called by name

`return this.function(message, this.handle)` (richards.ts:181) is checked
as `Callee::Prop("function")`: the checker knows `function` is a field of
the class with a fixed position and a function type, but the general
"function-valued property" path at the end of the method-call check
(`check/expr.rs:1620–1650`) emits a by-name call. At run time
`jit_call_named` then:

- tests `o.lookup(name)` (the class has more than 8 fields, so this hashes
  the string and probes the shape's map),
- falls to `lookup_method` → `get_chain` → `Object::get`, which hashes and
  probes again,
- clones the closure and calls it through `call_slice`.

Richards samples: `HashMap::get` 7.2%, `Object::get` 5.3%, `memcmp` 4.3%,
`lookup_method` 3.4% (20% together), on top of the generic call path of P1.

Recommendations:

1. In the checker, when the receiver is a class instance and the member is
   a field, emit a field read (by position) plus `Callee::Value` with
   `this` set to the receiver.
2. In `jit_call_named` and the `CallTarget::Property` arm of `do_call`, use
   the site's property cache (`site.cache.entry`, shape → slot) for own
   properties, as `ReadProperty` already does, before the slow lookup.

### P3. Start-up compilation

Best of 15 runs:

| Program | Default | `--no-jit` | Node |
|---------|---------|-----------|------|
| `console.log("hi")` (9 prelude functions compiled) | 5.2 ms | 2.5 ms | 67 ms |
| AWFY harness, one small benchmark (492 functions compiled) | 252 ms | 18 ms | 92 ms |

Synthetic programs of growing size (4 small functions per unit):

| Functions | Default | `--no-jit` | Ratio to previous |
|-----------|---------|-----------|-------------------|
| 509 | 140 ms | 9 ms | |
| 1,009 | 327 ms | 16 ms | 2.3× |
| 2,009 | 855 ms | 30 ms | 2.6× |
| 4,009 | 2,498 ms | 62 ms | 2.9× |
| 8,009 | 7,862 ms | 144 ms | 3.1× |

The front end (lex, parse, check, codegen) is linear, about 5 µs per source
line. Native compilation is quadratic. A profile of the 8,009-function
compile shows why: `compile` imports **every** compiled function into
**every** function (`for (g, gid) in &id_of { declare_func_in_func }`,
`lower.rs:2723`), and Cranelift then processes all of those signatures for
each function:

| Where | Samples (of 4,089) |
|-------|--------------------|
| `declare_func_in_func` and its hashing, in `compile` | 1,215 (30%) |
| `Context::clear` (freeing the imports again) | 420 (10%) |
| `SigSet::from_func_sig` / `compute_arg_locs` | 671 (16%) |
| `Verifier::run` (top of stack only) | 269 (7%) |
| Everything else, including real code generation | under 37% |

At the AWFY harness's size the quadratic part is about 10% of compile
time; the rest is per-function Cranelift cost, most of it spent on
functions that never run (the harness holds 14 benchmarks and runs one).

Recommendations, cheapest first:

1. **Turn off the verifier** in release builds
   (`flags.set("enable_verifier", "false")`; it is on by default). One line.
2. **Import only what a function calls.** Declare a callee in the function
   on first use (`Lower::funcs` becomes a lazily filled map). Removes the
   quadratic term.
3. **Skip native compilation for `--dump`** (it compiles everything, then
   prints bytecode: 258 ms on the harness).
4. **Compile on first call, or after a call count.** Eligibility is a
   whole-program fixpoint and stays as is (it is not visible in the
   profile); only `define_function` moves. A stub per function, or a check
   in `do_call`/`call_native_values`, triggers it. Direct calls between
   compiled functions need the callee compiled first or a patchable
   indirection; compiling a function's direct callees with it is the simple
   rule.
5. **One entry trampoline per signature**, not per function. Today every
   function compiles two Cranelift functions (`fN` and `eN`); trampolines
   depend only on the parameter and result kinds.
6. One function that Cranelift rejects makes `compile` return 0: the whole
   program runs interpreted, silently unless `RTS_JIT_DEBUG` is set. Skip
   that function instead, or at least say so.

### P4. Cycle collector

| Benchmark (standard size) | Run time | With `--no-cycles` | Collector pauses (`--mem-stats`) | Boxes it freed |
|---|---|---|---|---|
| DeltaBlue | 124 ms | 111 ms (−10%) | 11 pauses, 12.5 ms | 48,004 of 1.08 M |
| Havlak | 791 ms | 664 ms (−16%) | 88 pauses, 165.5 ms (21%) | 552,193 of 12.4 M |
| CD | 273 ms | 235 ms (−14%) | 78 pauses, 56.6 ms (21%) | 530,576 of 3.3 M |
| Richards | 467 ms | 522 ms (+12%, slower without it) | | |
| Json, Storage | no change | | | |

The collector is needed (it frees 4–16% of all objects) and its cost grows
with the live heap: at 5× DeltaBlue's problem size it is 35% of samples
(`Value::trace` 12.8%, `collect` 12.2%, `Object::trace` 5.0%, the
`children` closure 4.6%) against 11% at the standard size.

Recommendations (each needs measuring; none is certain):

1. **Tracing cost per box.** `children()` fills a `Vec` through a
   `&mut dyn FnMut` for every box, four times per collection (mark gray,
   scan, collect white, restore), and tracing an object first clones its
   shape `Rc` (`trace_shape(&self.shape(), t)`). A per-type "visit children
   with this operation" function in the vtable, with no intermediate
   vector and no shape clone, removes the indirect call per edge.
2. **Fewer candidates.** A box becomes a candidate on every decrement that
   leaves a non-zero count. Decrements of stack references (locals,
   temporaries, arguments) dominate count traffic in compiled code (about
   70% in Richards) and cannot create a garbage cycle on their own while
   another reference exists in a field. Deferring candidate registration to
   decrements that come from heap stores (`set_slot`, array writes, cell
   writes) and from frees is the standard refinement; it needs care to stay
   correct for a cycle whose last outside reference is a local.
3. **Trigger.** Collection starts at a candidate count (10,000, doubling to
   262,144 when under a quarter are garbage). Adding the live-heap size to
   the decision would stop long traces on large, mostly live heaps
   (DeltaBlue at 5×).
4. `Richards` running slower with the collector off suggests leaked cycles
   hurt the allocator; worth a look before tuning the trigger upward.

### P5. Strings

Json samples: `memcmp` 11.5%, `h_op` 9.0%, `strict_eq` 7.9%, malloc and
free 24.6%, `JsStr::new` 3.0%, `slice16` 2.9%, `h_drop_val` 2.1%.

1. **One-character strings.** `s[i]`, `charAt`, `at`, `for-of` over a
   string and `split("")` allocate a new string per character (two
   allocations each, see 2), which is then compared by content. A table of
   the 128 ASCII one-character strings makes these allocation-free and lets
   `===` succeed on the pointer test `strict_eq` already has.
2. **Two allocations per string.** `JsStr { s: Box<str>, ascii }` lives in
   a `Gc` box, so every string is a box plus a separate buffer. The object
   code already has the mechanism for a tail in the same box
   (`Gc::new_tail_in`); strings can use it.
3. **`===` on strings in compiled code** is a helper call (`h_op`). An
   inline pointer comparison first, then the helper, covers the table
   strings and interned constants.
4. **Non-ASCII strings are quadratic.** `len16`, `code_unit(i)` and
   `slice16` re-encode from the start on every call
   (`encode_utf16().nth(i)`), so `for (i < s.length) s.charCodeAt(i)` over
   a string with one non-ASCII character is O(n²), and `slice16` builds the
   whole UTF-16 vector for each character read. Cache the UTF-16 length,
   and either keep such strings as UTF-16 or keep a small index.
5. **`typeof x`** allocates a string each time (`Value::str(type_of())`,
   two allocations) and `typeof x === "string"` then compares contents.
   Preallocate the seven results.
6. `s += x` in a loop copies the whole string each time (no rope or
   builder). Standard engines defer this; worth doing only if a workload
   needs it.

### P6. Built-in calls allocate their argument list

`call_native(vm, n, this, args: Vec<Value>)` takes ownership of a vector.
The interpreter builds one per call (`collect_args`), compiled code copies
its argument slice (`args.to_vec()` in `jit_call_native`), and
`call_slice_unblocked` copies again for natives and bound functions. So
`map.get(k)`, `s.charAt(i)`, `arr.pop()`, `new Array(n)` each cost a
malloc and a free beyond their own work (Json: `call_native` 4.5% plus the
vector drop 2.4%; Storage's `new Array(n)` makes three allocations: the
argument vector, the array box, the element buffer, in a benchmark that
spends 44% of its samples in malloc and free).

Recommendation: natives take `this: &Value, args: &[Value]`. Most arms
already read arguments through `arg(&args, i)`; the few that consume the
vector (`ArrPush`, `ArrOf`, `ArrUnshift`) clone what they keep.

Related: `method_by_name` finds a built-in method by scanning up to 40
names with string comparisons on every by-name call on an array, string,
map or set (untyped code). Cache the result in the call site, keyed by the
receiver's tag.

### P7. Object size and allocation

`Object` is 48 bytes (`class: Option<Rc<ClassRt>>`, `shape: Rc<Shape>`,
`spill: Vec<Value>` at 24 bytes, `cap: u32`) after a 16-byte box header: 64
bytes before the first field. A two-field instance is 96 bytes. Creating
one increments two `Rc` counts (class and shape) and freeing it decrements
them; freeing also calls through the vtable to recompute the layout.

Recommendations:

1. Reach the class through the shape (class instances' shapes are per
   class already), make `spill` a thin optional pointer, keep `cap` in the
   shape: 8 bytes of object header instead of 48, and one count update
   instead of two. A two-field instance becomes 56 bytes.
2. Shapes and classes owned by the program live as long as it does; their
   counts need not be maintained per object.
3. Arrays are a box plus a separate buffer; small literal arrays
   (`[a, b]`, map entries, iterator results) could hold their elements in
   the box.

### P8. Shapes and dictionary-like objects

1. `Shape::make` always builds the name → index hash map, though `lookup`
   scans linearly for 8 keys or fewer. Build the map only above 8 keys.
2. `with_key` copies the key vector for each added key, so building an
   object with n dynamic keys is O(n²); above 64 keys transitions are not
   cached, so every insertion builds a complete new shape and map.
   `delete` is O(n) and builds a new shape. Objects used as dictionaries
   (`counts[word] = ...`) should switch to an own hash map after a
   threshold or the first `delete`.
3. `JSON.parse`, object rest, `structuredClone` and `Promise.withResolvers`
   call `Shape::new(keys)` per object: parsing 10,000 records creates
   10,000 shapes (each with a vector and a map) and defeats the inline
   caches. Build them through the transition tree from the empty shape.
   `JSON.parse` also checks duplicate keys with a linear scan per key.
4. `set_dyn` (`obj[key] = v`) allocates an `Rc<str>` for the key on every
   write, even when the key exists. Look up by `&str` first.

### P9. Array and JSON built-ins copy before working

- `indexOf`, `lastIndexOf`, `includes` call `to_vec()` first: a full copy
  with a count update per element, then the scan. `includes` also clones
  each element twice into `HKey`.
- `map`, `filter`, `forEach`, `find*`, `some`, `every`, `reduce` allocate
  an index vector (`(0..len).collect()`) per call and clone the array
  handle for each element's third argument.
- `shift` is `Vec::remove(0)`: a queue drained with `shift` is O(n²).
- `splice`, `concat`, `unshift`, `sort`, `flat`, `entries` copy the whole
  array; `join` builds a `Vec<String>` and joins it.
- `JSON.stringify` walks the value three times (BigInt check, `toJSON`
  check, output), copying each array and key list every time, and formats
  an indent string per nesting level even with no indent.

These matter only for programs that lean on the built-ins; each is a local
fix (iterate under the borrow, index loops without the vector).

### P10. Iterators, generators, promises

- `Vm::get_property(name)` builds a `PropertySite` (two allocations) per
  call. One step of a user-defined iterator calls it three times (`next`,
  `done`, `value`), plus the result object: about nine allocations per
  element. Keep static sites for the protocol names.
- `for-of` over a `Map`, `Set` or string, and spread, materialize the whole
  sequence first (`natives::iterate`); `get_iterator` on an array copies it.
- Suspending a generator moves every slot into the coroutine's vectors and
  resuming takes the vectors out (`mem::take`), so each `yield` frees and
  reallocates both buffers. Copy without taking.
- Timers are a `Vec` scanned for the minimum and removed by index on each
  tick: O(n) per timer. A binary heap if timer-heavy programs matter.

### P11. Interpreter

Most typed code is now compiled, so these matter for functions the
compiler rejects (`try`, object literals, generators, async) and for
`--no-jit`:

- Region instructions and arena allocations (`EnterRegion`, `ExitRegion`,
  `New*In`) run through `exec_extended`: a non-inlined call that fetches
  and matches the instruction a second time. Scope arenas pay this on
  every block entry and exit in the interpreter.
- `release_frame` visits every value slot of the frame on each return,
  temporaries included.
- The by-name call path clones the receiver (`this_counted`) before the
  cache test.
- `NewInstance`/`NewObject` clone the class and shape `Rc`s per allocation
  (see P7).

### P12. Smaller items

- **Allocator accounting.** `CountingAlloc` does three atomic updates and a
  peak check per allocation and one per free, in every build, for
  `--mem-stats`. With malloc/free at 24–44% of samples on Havlak, Json and
  Storage this is worth measuring (I did not; a guess is 1–3%);
  thread-local plain counters, or the `profile` feature, would do.
- **`x | 0` and friends** call `h_int32` from compiled code (3% of
  Bounce). An inline convert-and-compare with the helper as fallback.
- **Arena bookkeeping.** Each arena allocation pushes to a `RefCell<Vec>`
  of boxes; region enter/exit are helper calls in compiled code. Small
  next to P1 and P4; only 3% of CD's count updates touch arena boxes.
- **Release profile.** `lto = "thin"`; `"fat"` with the existing
  `codegen-units = 1` is a free experiment for the interpreter and
  runtime helpers.

---

## Simplifications

### S1. Side tables keyed by address, and HIR that gets cloned

The escape analysis identifies allocation sites and region blocks by the
address of the HIR node (`e as *const HExpr`, `ss.as_ptr()`), and the JIT
keys its `for-of` state by statement address. Codegen clones HIR in three
places: `inline_call` (`hf.body.clone()`, to satisfy the borrow checker),
and `HK::New` / `HK::NewBuiltin` (`args.clone()`, to build a synthetic
call). Clones have new addresses, so inside an inlined callback, and in
the arguments of a `new`, `region_of` finds nothing: those allocations
silently go to the heap in the interpreter while compiled code (which
works on the original nodes) uses arenas. Nothing breaks; the two back
ends just disagree, and nothing would report it.

Recommendation: stop cloning (borrow the body through the `'a` program
lifetime as `Codegen::func` already does; pass argument slices to the call
emitter), or give HIR nodes ids. Before enabling arenas in inlined bodies,
note that a "result region" site inside an inlined callback must not use
the enclosing function's result region.

### S2. One method lookup

The class-chain walk for a method name (`c.methods.get(name)`, then the
parent) is written four times: `do_call` (Property arm), `jit_call_named`,
`lookup_method`, and the `In` instruction. The method inline cache
(test, miss path, update) is duplicated verbatim in `do_call` and
`jit_call_named` (about 25 lines each). One `Vm::find_method(site, recv)`
serves both; flattening inherited methods into each class's map when
classes are built removes the walk.

### S3. One way to call a function by id

Five places allocate a `Closure { func, caps: [], props }` only to call a
known function: `jit_call_func` (fallback), `lookup_method`,
`get_prop_slow` (getter), `set_prop` (setter), `construct`. An internal
`call_function(fid, this, args)` removes the allocations and the
repetition.

### S4. Codegen repetition

- "Jump if truthy, by kind" appears four times (`cond_jump`, `Logical`,
  `LogicalAssign` twice), each with
  `if sense { …when: true… } else { …when: false… }`, which is
  `when: sense`.
- "Default value by kind" appears four times (`load_default`,
  `init_default`, `init_var`, `OptChain`).
- The list of instructions that carry a jump target is written twice
  (`patch`, `jump_target`).
- `std::collections::HashMap` (SipHash) is used here and `FxHashMap`
  everywhere else.

### S5. Instruction tables written by hand

`INSTRUCTION_KINDS`, `INSTRUCTION_NAMES` and `Instruction::kind()` are
three parallel 123-entry lists (260 lines of `bytecode.rs`). The helper
table and the natives list are already generated by a macro; instructions
can be too. `Native::is_task` and the `>= ProcessObject` test rely on
declaration order; an explicit group in the macro is safer.

### S6. Dump-only data on every run

`slot_names` (with a backwards linear search per slot allocation in
`note_slot`), `field_names`, `dump_name`, `param_names`, `capture_names`
and the global name lists are computed and stored for every function on
every run, and used only by `--dump`. Record them only when dumping.

### S7. Leftovers

- About 20 `let _ = x;` placeholders for values that are no longer used
  (`vm.rs:2235`, `2344`; `vm_task.rs:300`, `892` (`let _ = Instant::now()`);
  `codegen.rs:1347`, `2214`, `2873–2877` (a dead `map_num` block), `3034`,
  `3130`; `jit/lower.rs:2857`; nine in `check/`).
- `smallvec` is a dependency that nothing uses.
- Three `#[allow(dead_code)]`.
- In `FunctionCode`, the doc comment for `borrowed` sits on `jit`.
- Stray indentation where code was removed (several `tri!(r)` lines in
  `vm.rs`).
- `vm.rs:1190`: `borrowed & (1 << vi) != 0 && vi < 64` shifts before it
  checks; put the bound first.
- `README.md` still quotes an old geometric mean (0.494 s → 0.237 s).
- `target/base`, `target/head`, `target/prev` (1.1 GB) are stale build
  directories.

### S8. Lints, formatting, tests

- `cargo clippy` fails: 5 errors (public functions that dereference a raw
  pointer without being `unsafe`, `src/jit/mod.rs:287–312`) and 26
  warnings. Make those functions `pub(crate) unsafe`, fix or allow the
  rest, and keep it at zero.
- `cargo fmt --check` reports 1,262 differing hunks. Either adopt rustfmt
  with a `rustfmt.toml` that matches the wide lines in use, or drop the
  pretence; a mix makes every diff noisier.
- `tests/run.sh` runs one configuration. I ran the 25 programs under 11:
  default, `--no-jit`, `--no-arena`, both, `RTS_CC_THRESHOLD=1` (with and
  without the JIT), the tracing-GC build, `RTS_GC_MIN_HEAP=0` (with and
  without the JIT), `RTS_ARENA_CHECK=1`, and `--no-types`. All pass except
  `--no-types` (below). The script should loop over these: interpreter
  against compiled code is a free differential test, and it is what would
  have caught the narrowed-capture bug earlier.
- The script writes to fixed files in `/tmp`; there are 3 unit tests and no
  CI.
- The repository has no `CLAUDE.md` of its own; `~/CLAUDE.md` prescribes a
  workflow (beads) this project doesn't use, and has empty build and test
  sections. A short project file with the build directories
  (`target/release`, `target/gc`, `target/profile`), `tests/run.sh`,
  `bench/awfy/compare.sh` and `bench/measure.sh` would replace what is
  currently carried in session memory.

### S9. Four idioms for single-threaded global state

`static` + `UnsafeCell` + `unsafe impl Sync` (heap statistics, the cycle
collector, the tracing GC, the JIT's region stack), `thread_local!` with
`RefCell` (arena chunk pool, iterator-result shape, profile sites, the
random state), plain `static mut` flags in the JIT, and fields of `Vm`.
One wrapper type for "owned by the VM thread", or one runtime struct
reachable from `Vm` and `JitCtx`, would make the rule visible and remove
most of the `unsafe impl Sync` blocks.

### S10. File layout

- `runtime/mod.rs` (1,300 lines) holds values, strings, arrays, shapes,
  objects, number formatting and 300 lines of `console.log` formatting.
  `inspect.rs`, `object.rs` (shapes and objects) and `number.rs` are
  natural splits.
- `vm_task.rs` is included with `#[path]` as `vm::task` and holds scope
  regions, the event loop, promises, generators, iteration, `construct`
  and `instanceof`. Regions belong with the arena code; the rest splits
  into `promise.rs` and `iter.rs`.
- `natives.rs` is one 1,000-line `match`; grouping by receiver (string,
  array, map/set, JSON, object) would also give P6 and P9 natural seams.

### S11. Classes recognized by name

`Date` is detected by comparing the class name string (`to_number`,
`inspect`, `json_stringify`, `json_needs_prepare`), and `is_error_class`
walks the parent chain comparing names. The program already records class
ids for the error classes; record `Date`'s the same way and compare ids.

---

## Defects found on the way

Not part of the brief, listed because they surfaced.

1. `--no-types`: `xs[-1] = v` on an array throws a `TypeError`
   (`set_dyn` returns false for a non-index key); Node stores a property.
   (`jit_arrays.ts`)
2. `--no-types`: `for (const x of xs)` over an array that grows during the
   loop sees a snapshot (3 iterations instead of 5); the typed path walks
   the array live. (`jit_statements.ts`)
3. `--no-types`: `this` is `undefined` inside a static method called as
   `this.make(3)`. (`jit_statements.ts`)
4. `cargo clippy` fails (S8).
5. One function rejected by Cranelift disables native code for the whole
   program without a message (P3.6).

## What looked fine

- The front end is linear and fast (144 ms for 28,000 lines).
- The eligibility fixpoint does not show up in the compile profile.
- Benchmarks without polymorphic calls (Bounce, List, Towers, Permute)
  spend 94–100% of their time in compiled code.
- Every memory mode and stress setting passes the test programs.

---

## Progress log

- `3f6562b` (P3 parts 1–3, P3.6, P2): callees imported on first use and
  the verifier off in release builds (AWFY harness start 252 → 166 ms;
  8,000 functions 7.9 s → 1.2 s, now linear); `--dump` skips native code;
  a Cranelift failure is reported. `this.f(...)` on a field holding a
  function reads it by position (typed Richards −30%). Fixed on the way:
  constructors with an untyped `this` got a one-word `this`, a verifier
  error that had silently kept the untyped JS suite fully interpreted.
- `119d143`: untyped operators (`+`, arithmetic, comparisons, `-x`, `~x`,
  ToNumber), `obj[key]` reads and writes and compound assignment on
  properties compile (numbers inline, the rest through helpers sharing
  new `Vm` methods with the interpreter); `set_prop` tries its cache
  first. Untyped JS suite vs. the interpreter: every benchmark faster,
  −14% (NBody) to −58% (Mandelbrot), CD −33%, Havlak −18%.
- `5a63afe`: virtual calls with several compiled implementations dispatch
  on the receiver's class in compiled code (DeltaBlue −22%, Havlak −4%).
  `cargo clippy` passes again. `RTS_JIT_DEBUG` lists call sites that still
  go through the interpreter.
- Next commit: calls through function values whose type fixes the kinds
  find a compiled callee in a table by function id and call it directly
  (Richards a further −23%; −46% since the review). New test
  `jit_calls.ts`. Gaps found while writing it (front end, not changed):
  `this:` parameter annotations don't parse; an arrow function whose body
  only throws is inferred as `void` instead of `never`; a method call on a
  plain object cast to a class type throws instead of calling the
  object's property (interpreter and compiled code agree).
- Adapters (`be8f79e`): each function callable through a function value
  also gets an adapter taking values (checked and unboxed inline), for
  call sites whose kinds differ, such as generic callbacks (DeltaBlue −12%).
- By-name method calls: an inline cache per call site in compiled code
  (receiver shape and class → method adapter); a miss calls the resolved
  method directly and fills the cache. Typed CD −17%; untyped JS suite
  −18% to −39% on 12 of 14 benchmarks (NBody, Sieve, Mandelbrot
  unchanged). P1 is now done for virtual, closure and by-name calls.
- P5 (parts 1, 3, 5): one-character ASCII strings and `typeof` results
  are shared (reference-counting build); compiled `===` decides strings
  inline when they are the same box, differ in length, or are one byte
  long. Json −22% typed, −12% untyped. New test `string_chars.ts`.
- P6: built-ins take `&[Value]` (compiled code, callbacks and bound calls
  no longer copy arguments into a vector); the interpreter collects call
  arguments in a `SmallVec` (inline up to four). Json −15%, Storage −11%
  typed; Json −8%, Storage −6% untyped.
- P4 (part 1): tracing an object no longer clones its shape's `Rc`;
  collector passes visit children directly instead of copying them into
  a vector; `Value::trace` is one tag test. CD −8%, Havlak −4%,
  DeltaBlue −2%. The trigger threshold was already close to the best
  fixed setting for Havlak and DeltaBlue (measured at 50K, 200K, 1M
  candidates), so the remaining cost is the tracing work itself; parts
  2–3 (fewer candidates, heap-size-aware trigger) remain open.
