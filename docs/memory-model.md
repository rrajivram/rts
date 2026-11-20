# Memory model: ownership instead of a cycle collector

Status: prototype. The heap census (`RTS_CENSUS`, `bench/census.py`, results
in `bench/results/census.json` and `census-owners.json`) motivated the design.
It is built as `--features owners`, measured below, and isn't the default yet.

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

## Prototype: `--features owners`

Built as a separate binary (`cargo build --release --features owners
--target-dir target/owners`), so the default build is unchanged. The global
cycle collector doesn't run in it.

- `heap.rs`: an `owner` word in the box header (+8 bytes, so the header is
  24 bytes, the same as the tracing GC's), and `OWNED`/`OWNS` flags.
  - `own` on a store, `disown` before a reference leaves its holder, `adopt`
    on a load, and `orphan_children` when a holder is freed.
  - With `OWNED` in the quiet mask, a decrement of an owned box never
    queues it as a candidate, even from inline compiled code.
- Hooks in every store and remove path for object fields, array elements
  and captured-variable cells: interpreter, natives (`pop`, `shift`,
  `splice`, `sort`, `fill`, …), and compiled code's `store_slot`, field and
  element loads.
- `cycles::collect_owned`: trial deletion over the candidates, what they
  own, and the unowned roots they borrow. Never over owned boxes reached by
  a borrow. Checks run at 10 000 candidates, or once the program's heap has
  grown by a quarter (at least 512 KB) while candidates wait.
- The census verifies the invariant: in the owners build it counts owner
  links that don't match a real reference. It found none on any program.

The prototype changed the design in three places. Each came from
measurement, and all three are in the code:

1. **Demote on cycle.** Structures are often built bottom-up: `grow()`
   makes a child with `parent = n` before `n` holds the child, so the child
   became the parent's owner and the parent's array couldn't own the child.
   Now, when a store would close an ownership cycle, the store wins and the
   older link becomes a borrow. The census simulation had hidden this: it
   assigned owners after construction, top-down.
2. **Adopt on load.** When a `Vector` grows, it copies its elements into a
   new array and drops the old one, which orphans them. The new array
   already holds them, so nothing re-owned them, and Havlak's 5 213 blocks
   were queued as candidates 349 842 times. Reading an unowned object out of
   a holder proves the holder holds it, so the holder adopts it. A load never
   demotes a link.
3. **Checks follow borrows to unowned roots.** DeltaBlue's graph is many
   small trees (each Variable owns its constraint vector) linked by borrows
   (constraints point at other Variables). A middle Variable was checked
   while a loop variable still held it, survived, and nothing ever queued it
   again. A check now also takes in the unowned roots its members borrow, so
   a dead structure of linked trees is checked as one. Owned boxes reached
   by a borrow stay out of the check, so a large live structure is never
   pulled in.

### Results (`bench/memory.py`: live heap every 250 µs, every pause)

Memory MB, mean / peak, and the longest pause in ms; 5 iterations at standard
sizes. Only the memory strategy differs.

| program | RC today | **RC, ownership** | tracing GC, default | tracing GC, tuned small |
|---|---|---|---|---|
| Havlak | 24.2 / 38.3, 14.7 | **13.4 / 15.7, 3.7** | 30.8 / 60.0, 16.6 | 20.8 / 35.6, 9.8 |
| DeltaBlue | 11.7 / 19.7, 6.5 | **8.6 / 14.0, 6.1** | 17.1 / 37.7, 9.9 | 12.4 / 25.7, 11.2 |
| CD | 1.26 / 2.09, 0.71 | **0.90 / 1.58, 0.36** | 5.47 / 10.3, 2.8 | 2.80 / 5.25, 1.29 |
| churn | 4.00 / 5.70, 2.8 | **1.00 / 1.29, 0.35** | 4.74 / 7.79, 1.6 | 1.25 / 1.67, 0.35 |
| Richards | 0.56 / 1.19, 0.48 | **0.32 / 0.76, 0.24** | 0.84 / 2.01, 0.52 | 1.21 / 2.38, 0.04 |
| Json | 0.69 / 1.06, 0.28 | **0.64 / 1.33, 0.19** | 7.88 / 14.6, 4.7 | 4.22 / 7.46, 2.5 |
| Storage | 0.55 / 1.04, 0.25 | **0.56 / 1.09, 0.21** | 4.89 / 9.34, 1.8 | 2.59 / 4.97, 3.6 |
| requests | 2.33 / 13.8, 0.10 | **2.22 / 13.3, 0.60** | 5.58 / 18.4, 3.7 | 3.27 / 16.3, 1.5 |

- **Memory: lowest of every strategy on the cyclic programs.** On
  Havlak, DeltaBlue, CD, churn and Richards, ownership is 25–75% below
  today's RC (Havlak's peak falls from 38 to 16 MB) and 1.3–3.8× below the
  GC tuned for small memory. On the acyclic ones (Json, Storage, requests)
  it ties today's RC (Json's peak is 0.3 MB higher) and stays 1.5–6× below
  the tuned GC. These figures include the 8 extra header bytes.
- **Pauses: much shorter, not yet gone.** Havlak's longest falls from 14.7
  to 3.7 ms (the GC's: 9.8–16.6). DeltaBlue's is still 6.1 ms: a check of a
  dead 96 000-object constraint graph, traced and freed in one go. The cost
  is proportional to the garbage, as intended, but still a single pause.
- The census confirms it: DeltaBlue holds within 9% of exact, and Havlak,
  Json, Storage and requests hold exactly what an exact collector would.

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

1. ~~Prototype owners and the orphan check.~~ Done (above). The census
   replaced the cycle collector as the cross-check: it compares what's held
   with what's reachable and verifies every owner link.
2. ~~Unowned-root checks, cycle collector off.~~ Done in the owners build.
   Memory target met. The pause target (≤ 1 ms everywhere) is not: Havlak
   3.7 ms, DeltaBlue 6.1 ms.
3. **Bound each check.** The remaining pauses are single checks over large
   sets: a dead constraint graph, or unowned roots with large trees. Options:
   - free the garbage incrementally (the freeing half of the pause);
   - cap a check's size and split it, which needs to know that nothing
     mutated the part already traced;
   - find why Havlak still has unowned roots with large trees (ownership
     missed somewhere).
4. **Make ownership the default build**, once throughput is measured
   (`bench/modes.py`, config `rc-owners`).
