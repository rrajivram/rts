# Memory model: ownership instead of a cycle collector

Status: proposal, backed by the heap census (`RTS_CENSUS`, `bench/census.py`,
results in `bench/results/census.json`). Nothing here changes how memory is
managed yet.

## The thesis, and why the current design can't show it

The claim is that reference counting beats tracing GC **on memory**: memory
that stays close to what the program uses, and no collector pauses. Not on
throughput.

Today's model counts every object, and a Bacon–Rajan cycle collector reclaims
what counting can't. That collector *is* a tracing collector over the
candidate subgraph: it pauses (11.6 ms on Havlak, 6.0 ms on DeltaBlue), lets
garbage pile up between runs, and its pause grows with the structure. So "RC
beats GC" reduces to "RC beats GC except where the hard case is handed to a
GC". Tuning it (thresholds, fewer candidates, deferred counts) moves that
line; it doesn't remove it.

## The census: what's actually in the heap

`src/runtime/census.rs` registers every heap box and, at safepoints, traces
the heap:

- **Kinds.** A box's kind is its class (or `{keys}` for a plain object,
  `Function`, `Cell`, …); arrays, maps and sets are transparent. A kind is
  *cyclic* if it sits in a cycle of the kind graph: what a type-graph analysis
  of the program would find.
- **Reachability.** Boxes whose count exceeds the heap references to them are
  roots; what they reach is what an *exact* collector would hold.
- **Regions, simulated.** Boxes of cyclic kinds (and containers holding them)
  are grouped into regions, freed whole once no member is reachable. Two rules
  for forming regions:
  - *merge*: any reference between two boxes puts them in one region;
  - *owner*: a box joins the region of what first refers to it; regions merge
    only when references between them form a cycle.

  What a design holds is reachable bytes plus *floating garbage* (dead or
  unreachable members of regions still held).
- **The cycle collector.** How much of each collection's tracing starts from
  candidates of cyclic kinds: tracing a better static `acyclic` analysis could
  never remove.

### Results

Memory in MB, mean / max over snapshots (5 iterations, standard sizes):

| program | RC + cycle collector (today) | exact | regions, merge | regions, owner |
|---|---|---|---|---|
| Havlak | 10.35 / 16.36 | 7.35 / 9.09 | 33.94 / 72.65 | **7.76 / 9.58** |
| DeltaBlue | 5.99 / 12.14 | 4.57 / 8.80 | 4.57 / 8.80 | **4.57 / 8.80** |
| CD | 0.69 / 1.25 | 0.32 / 0.55 | 0.33 / 0.55 | **0.33 / 0.55** |
| Richards | 0.43 / 0.71 | 0.03 / 0.04 | 0.03 / 0.04 | **0.03 / 0.04** |
| Json | 0.21 / 0.39 | 0.21 / 0.39 | 0.21 / 0.39 | **0.21 / 0.39** |
| Storage | 0.01 / 0.01 | same | same | same |
| requests | 0.01 / 0.01 | same | same | same |
| churn (hard case) | 2.28 / 4.41 | 0.37 / 0.38 | 33.59 / 69.11 | **11.43 / 22.81, growing** |

Cycle collector: share of its tracing that starts from cyclic-kind candidates.

| Havlak | DeltaBlue | CD | Richards | churn |
|---|---|---|---|---|
| 93.0% | 96.5% | 99.9% | 99.8% | 99.9% |

What this says:

1. **Static acyclicity is a dead end.** 93–99.9% of the collector's work
   starts at objects whose types really are cyclic. A better type analysis
   would remove almost none of the pauses.
2. **Today's RC holds 30–1300% more than an exact collector** on the cyclic
   programs: garbage cycles wait for the collector. RC isn't "constant and
   tight" here; the collector is the problem, not the counting.
3. **Ownership is the right structure.** Grouping by *first referrer* matches
   exact memory within 6% on every AWFY program and the request workload,
   with no tracing. Grouping by *any reference* (the obvious region design)
   is 4× worse on Havlak: temporaries that point into the long-lived CFG get
   welded to it. Direction matters.
4. **Regions alone aren't enough.** `bench/workloads/churn.ts`: a long-lived
   tree with parent pointers keeps losing whole subtrees. Each pruned subtree
   is a garbage cycle *inside* a live region, so whole-region freeing never
   reclaims it, and memory grows without bound. The design needs a way to
   reclaim a dead piece of a live structure, at a cost proportional to the
   piece.
5. Robustness: Havlak with snapshots 24× more often gives the same picture
   (owner 8.37 / 9.68 vs exact 7.82 / 9.09). JSON, Storage and the request
   workload have no garbage cycles at all: exact RC already wins there.

## The design: ownership reference counting

Keep exact reference counting for every object: it frees acyclic garbage the
moment it dies, which is most garbage. Replace the cycle collector with
**ownership**:

1. **Every object has an owner**: the box holding its first heap reference (a
   field, an element, a captured cell). Every other reference is a *borrow*.
   Owner links form a forest; an ownership tree is a region. Objects held only
   by variables or globals are unowned *roots*.
   - A store makes the holder the owner only if the object has none and the
     holder isn't owned (transitively) by it. An object that owns nothing
     can't be an ancestor, so the common case, a fresh object stored once, is
     O(1); otherwise walk the holder's owner chain, with a bound, and leave
     the object unowned if the bound is hit.
   - Objects of acyclic kinds never need owners: exact RC is enough for them.
2. **Losing the owner triggers a local check.** When the owning reference
   goes away (overwritten, removed, or the owner freed) and the object's count
   is still above zero, everything orphaned in that step is checked together.
   Take the owned subtrees of the orphans, compare each member's count with
   the references from inside the set, and trace from the members referenced
   from outside, within the set only. Free what isn't reached; survivors
   become unowned roots (re-owned by their next store).

   The cost is proportional to what was orphaned, which is usually what's
   being freed: the same order as an RC free cascade, not the live heap. A
   pruned subtree (churn) or a dropped structure (DeltaBlue's constraint
   graph, CD's trees, Havlak's loop-finder state) is checked exactly once, as
   it dies.
3. **Unowned roots whose count drops to non-zero** get the same check over
   their ownership tree. This is the remaining collector-like case. The bet,
   to be measured, is that true roots are few and their counts rarely change:
   most structures are owned by some object, and borrows to owned objects
   never trigger anything.
4. **Freeing is incremental.** Everything a count or a check frees goes on a
   queue drained with a fixed budget per allocation. Memory is still returned
   promptly, but no single drop, however large the structure, is a pause.
   This also caps RC's own free cascades.
5. **No global collector.** The Bacon–Rajan collector stays only as a
   debugging cross-check (does it ever find garbage that ownership missed?)
   and is off by default.

Why this serves the thesis: memory tracks the exact live set (census rule
*owner*, plus the orphan check for the churn case), and there's no operation
whose cost depends on the size of live data. Checks touch what's being
orphaned, and frees are budgeted.

Relation to prior work: Bacon–Rajan trial deletion, restricted to the owned
set and triggered by losing ownership rather than by any decrement. The
ownership forest is in the spirit of Lobster's inferred ownership and of
Rust/Vale owner-plus-borrow, but inferred dynamically and never rejected.
Programs keep full JavaScript semantics.

## Risks and open questions

- **Ownership churn.** An object moved between containers (popped from one
  worklist, pushed onto another) loses its owner each time; the check costs
  the size of its owned subtree. Cheap for leaves, expensive if large owned
  structures move around. Measure checks per second and their sizes.
- **Unowned roots.** If a large structure stays unowned and its count changes
  often (a root passed around through variables that are counted), every
  change is a check over the whole tree. Possible fixes: frame-owned roots,
  or not counting borrows from variables at all (borrowed parameters already
  aren't).
- **Space.** An owner link per object of a cyclic kind (8 bytes; acyclic
  kinds need none). That fights the memory thesis; alternatives are a side
  table, or owner-by-page.
- **Every store path must maintain owners**: interpreter, compiled code's
  inline stores, natives (array methods, `Map`, `Set`, closures' cells).
  Missing one leaves objects unowned, which costs checks, not correctness.
- **Short-lived cyclic objects** (Havlak: 400 MB of them died between
  snapshots) weren't in the simulation. Most die by count alone, but this
  is what the prototype must confirm.

## Plan

1. **Prototype owners and the orphan check, with the cycle collector still
   running as a cross-check.** Measure: garbage the collector still finds
   (target ≈ 0), checks per second, objects traced per check, the longest
   check.
2. **Unowned-root checks, then the cycle collector off by default.** Rerun
   `bench/memory.py` and `bench/latency.py`. Target: on every program, memory
   within 10% of exact and no pause above 1 ms (today 11.6 ms on Havlak, 6.0
   ms on DeltaBlue).
3. **Budgeted incremental freeing** for counts and checks alike.
4. **Fold the census into the report**, so the "exact vs held" gap is tracked
   with every step.
