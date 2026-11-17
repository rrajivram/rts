# Simplifying the native-code work

A cleanup pass over `src/jit.rs` (4,779 lines, now `src/jit/`) and its VM entry points,
after closures, `switch`/`for-of`, static methods and scope arenas landed.
No behaviour change is intended: every step must keep `tests/run.sh`
(release, debug, tracing-GC builds) and `bench/awfy/compare.sh` passing,
and steps touching hot paths are timed against the previous commit.

Order: the safe mechanical steps first, the ones that remove concepts
after, the file split last so it doesn't obscure the other diffs.

| # | Step | Status |
|---|------|--------|
| 1 | Leftovers from refactors: `stmt`/`stmt2` back into one match, stray braces in `loop_stmt`, `fast_mod` wrapper | done |
| 2 | Declare each runtime helper once (name, function, signature) instead of three parallel lists indexed by position | done |
| 3 | Fold repeated patterns: "hold the left operand across the right", inline-or-helper count updates, the null/undefined/heap tag check, the shared argument prefix in `call_native_code`/`call_native_values` | done |
| 4 | One variable-write path (`write_var`) beside `read_var`; one arm per place kind for `+=` and `++` | done |
| 5 | One region stack: compiled code uses the VM's regions and pool instead of its own | not done (see log) |
| 6 | Split `jit.rs` into `jit/` modules: eligibility, helpers, lowering, driver, profile counters | done |

Considered and left for later: replacing the profile-only `#[track_caller]`
site attribution with explicit labels (it only affects profile output), and
dropping the `declared` set for captured variables (it mirrors the
interpreter's rule exactly; changing it risks a semantic difference).

## Log

- Step 1: `stmt` is one match again; `loop_stmt` lost its stray braces; `fast_mod` and a redundant `%` branch in `dyn_num` are gone (`js_mod` is the one implementation).
- Step 2: a `helpers!` table declares each runtime helper once (field, Rust function, signature); the symbol list, signature list and `Helpers` construction are generated from it (about 90 lines fewer, no positional indices).
- Step 3: `hold_across` replaces six copies of the keep-alive rule (comparisons, concat, field and element bases); `val_count` is the one inline-or-helper count update for values (`inc_val`, `release_old`), and no longer emits an empty branch in the tracing-GC build; `unbox_ref` is the one null/undefined/object check (`coerce`, `load_slot`); `JitFn::write_prefix`/`write_arg` build the argument words for both interpreter entry points. Timing unchanged (best of 7, within 0.6%).
- Step 4: `write_var` stores into a variable wherever it lives (register, global, cell); assignment, `Init` (one arm instead of two), `MakeClosure` and the `for-of` binding use it. Numeric `+=` and `++` share `num_place`/`num_load`/`num_store` (one arm per place kind instead of four per operator). Merging `MakeClosure` fixed a latent ordering bug: when it had to make the variable's cell itself, it built the closure first, so a self-referencing local function would have captured the old cell; codegen makes the cell first. It couldn't be triggered from the programs tried (the enclosing block always made the cell), but the self-referencing cases are now in `jit_closures.ts`.
- Step 5, not done: on a closer look it adds coupling instead of removing it. The interpreter's regions belong to interpreter frames and are freed when a frame pops (`pop_regions`); compiled code's belong to a native call and are freed by the code itself. On the VM's stack a compiled region would need an interpreter frame index, and with no frame active (a timer callback calling a compiled closure) a nested interpreted call would push frame 0 and, on popping it, free the compiled code's open region. Unwinding would also need the VM's region count (a helper call on entry instead of today's inline load). Two stacks with different owners are the simpler design.
- Step 6: `src/jit.rs` is now `src/jit/`: `mod.rs` (context, value kinds, `JitFn`; 375 lines), `elig.rs` (801), `lower.rs` (lowering and `compile`; 2,951), `helpers.rs` (546), `rcsites.rs` (profile only; 62). Items used across modules are `pub(super)`. Profile output names `lower.rs` lines.

Result: about 90 fewer lines (2,183 removed, 2,092 added, counting the new module headers), three parallel helper lists and the positional indices gone, one write path for variables, one keep-alive rule, one value-count update, one reference check, one argument writer for both interpreter entries, and a latent closure-cell ordering bug fixed. No measurable timing change at any step (best-of-3 and best-of-7 comparisons against the previous commit).
