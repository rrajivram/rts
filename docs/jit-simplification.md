# Simplifying the native-code work

A cleanup pass over `src/jit.rs` (4,779 lines) and its VM entry points,
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
| 5 | One region stack: compiled code uses the VM's regions and pool instead of its own | pending |
| 6 | Split `jit.rs` into `jit/` modules: eligibility, helpers, lowering, driver, profile counters | pending |

Considered and left for later: replacing the profile-only `#[track_caller]`
site attribution with explicit labels (it only affects profile output), and
dropping the `declared` set for captured variables (it mirrors the
interpreter's rule exactly; changing it risks a semantic difference).

## Log

- Step 1: `stmt` is one match again; `loop_stmt` lost its stray braces; `fast_mod` and a redundant `%` branch in `dyn_num` are gone (`js_mod` is the one implementation).
- Step 2: a `helpers!` table declares each runtime helper once (field, Rust function, signature); the symbol list, signature list and `Helpers` construction are generated from it (about 90 lines fewer, no positional indices).
- Step 3: `hold_across` replaces six copies of the keep-alive rule (comparisons, concat, field and element bases); `val_count` is the one inline-or-helper count update for values (`inc_val`, `release_old`), and no longer emits an empty branch in the tracing-GC build; `unbox_ref` is the one null/undefined/object check (`coerce`, `load_slot`); `JitFn::write_prefix`/`write_arg` build the argument words for both interpreter entry points. Timing unchanged (best of 7, within 0.6%).
- Step 4: `write_var` stores into a variable wherever it lives (register, global, cell); assignment, `Init` (one arm instead of two), `MakeClosure` and the `for-of` binding use it. Numeric `+=` and `++` share `num_place`/`num_load`/`num_store` (one arm per place kind instead of four per operator). Merging `MakeClosure` fixed a latent ordering bug: when it had to make the variable's cell itself, it built the closure first, so a self-referencing local function would have captured the old cell; codegen makes the cell first. It couldn't be triggered from the programs tried (the enclosing block always made the cell), but the self-referencing cases are now in `jit_closures.ts`.
