//! Heap census (`RTS_CENSUS=file.json`): measurements for the memory-model
//! decision (docs/memory-model.md), not part of how memory is managed.
//! Reference-counting build only.
//!
//! Every heap box is registered when allocated and unregistered when freed.
//! At safepoints, every `RTS_CENSUS_EVERY` allocations (default: half the
//! live boxes, at least 10 000), a snapshot traces the whole heap and:
//!
//! - **Kind graph.** Each box has a kind: its class, `{keys}` for a plain
//!   object (its keys at birth), `Function`, `Cell`, … Containers (arrays,
//!   maps, sets) are transparent: `A → Array → B` is the kind edge `A → B`.
//!   A kind is *cyclic* when it is in a strongly connected component of
//!   that graph with an edge inside it (what a type-graph analysis finds).
//! - **Reachability.** A box whose count exceeds the references the census
//!   sees from other boxes is referenced from outside the heap (stack, VM,
//!   arenas): a root. Boxes reachable from roots are what an exact
//!   collector would keep; the rest is garbage this build still holds
//!   (cycles waiting for the cycle collector).
//! - **Region designs, simulated.** Boxes of cyclic kinds, and containers
//!   holding them, live in regions. A region is freed whole once none of
//!   its members is reachable; until then its dead and unreachable members
//!   are *floating garbage* the design holds and an exact collector
//!   wouldn't. Two rules for forming regions:
//!   - `merge`: any reference between two boxes puts them in one region;
//!   - `owner`: a box joins the region of what first refers to it (here:
//!     at the first snapshot that sees it), and regions merge only when
//!     references between them form a cycle. A reference into another
//!     region is a counted cross-region reference.
//! - **Cycle collections.** For each: candidates, boxes traced, garbage
//!   found, time; garbage by kind; and how many boxes a collection would
//!   trace if only candidates of cyclic kinds were buffered.
//!
//! Written as JSON at exit, with a summary on stderr.

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use rustc_hash::{FxHashMap, FxHashSet};

use super::heap::{flag, trace_header, Gc, Header, Tracer, STATS};
use super::Object;

static ON: AtomicBool = AtomicBool::new(false);

const NONE: u32 = u32::MAX;
const RULES: usize = 2;
const RULE_NAMES: [&str; RULES] = ["merge", "owner"];

#[derive(Clone, Copy)]
struct Info {
    kind: u32,
    bytes: u32,
    /// Union-find node per rule (`NONE`: not in a region).
    uf: [u32; RULES],
}

#[derive(Default, Clone, Copy)]
struct Region {
    members: u64,
    live: u64,
    live_bytes: u64,
    dead_bytes: u64,
}

/// One rule's regions: a union-find forest over boxes.
#[derive(Default)]
struct Regions {
    parent: Vec<u32>,
    regions: Vec<Region>,
}

impl Regions {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let p = self.parent[x as usize];
            self.parent[x as usize] = self.parent[p as usize];
            x = p;
        }
        x
    }
    fn fresh(&mut self, bytes: u64) -> u32 {
        let n = self.parent.len() as u32;
        self.parent.push(n);
        self.regions.push(Region { members: 1, live: 1, live_bytes: bytes, dead_bytes: 0 });
        n
    }
    fn union(&mut self, a: u32, b: u32) -> u32 {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return ra;
        }
        let (big, small) = if self.regions[ra as usize].members >= self.regions[rb as usize].members { (ra, rb) } else { (rb, ra) };
        self.parent[small as usize] = big;
        let sm = std::mem::take(&mut self.regions[small as usize]);
        let g = &mut self.regions[big as usize];
        g.members += sm.members;
        g.live += sm.live;
        g.live_bytes += sm.live_bytes;
        g.dead_bytes += sm.dead_bytes;
        big
    }
}

#[derive(Default)]
struct KindStat {
    allocated: u64,
    bytes: u64,
    candidates: u64,
    garbage: u64,
}

struct Collection {
    at_alloc: u64,
    candidates: u64,
    traced: u64,
    garbage: u64,
    ns: u64,
    cyclic_candidates: u64,
    /// Boxes traced from the candidates of cyclic kinds only.
    traced_cyclic_only: u64,
}

#[derive(Clone, Copy, Default)]
struct RuleSnap {
    /// Reachable bytes plus floating garbage: what the design holds.
    design: u64,
    floating: u64,
    /// The largest region with a reachable member: live bytes, members.
    largest: u64,
    largest_members: u64,
    regions: u64,
}

struct Snap {
    at_alloc: u64,
    /// What this build holds (registered boxes).
    live: u64,
    /// What an exact collector would hold.
    reachable: u64,
    rules: [RuleSnap; RULES],
}

#[derive(Default)]
struct State {
    path: String,
    every: Option<u64>,
    next: u64,
    boxes: FxHashMap<usize, Info>,
    live_bytes: u64,
    kinds: Vec<String>,
    kind_ids: FxHashMap<String, u32>,
    /// (vtable, class) → kind, for boxes whose kind doesn't depend on keys.
    kind_cache: FxHashMap<(usize, usize), u32>,
    kstats: Vec<KindStat>,
    /// Kind graph edges (containers transparent).
    kedges: FxHashSet<(u32, u32)>,
    cyclic: Vec<bool>,
    rules: [Regions; RULES],
    region_frees: [u64; RULES],
    /// Cyclic-kind boxes freed before any snapshot put them in a region.
    unseen_cyclic: u64,
    unseen_cyclic_bytes: u64,
    collections: Vec<Collection>,
    snaps: Vec<Snap>,
}

struct Global(UnsafeCell<Option<State>>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}
static STATE: Global = Global(UnsafeCell::new(None));

#[allow(clippy::mut_from_ref)]
fn st() -> &'static mut State {
    unsafe { (*STATE.0.get()).as_mut().unwrap() }
}

#[inline(always)]
pub fn on() -> bool {
    ON.load(Relaxed)
}

pub fn start_from_env() {
    let Ok(path) = std::env::var("RTS_CENSUS") else { return };
    let every = std::env::var("RTS_CENSUS_EVERY").ok().and_then(|v| v.parse().ok());
    unsafe { *STATE.0.get() = Some(State { path, every, next: 10_000, ..Default::default() }) };
    ON.store(true, Relaxed);
}

// ───────────── kinds ─────────────

fn intern(s: &mut State, name: String) -> u32 {
    if let Some(&k) = s.kind_ids.get(&name) {
        return k;
    }
    let k = s.kinds.len() as u32;
    s.kinds.push(name.clone());
    s.kind_ids.insert(name, k);
    s.kstats.push(KindStat::default());
    s.cyclic.push(false);
    k
}

fn object_of(p: NonNull<Header>) -> &'static Object {
    unsafe { &*((p.as_ptr() as *const u8).add(Gc::<Object>::VALUE_OFFSET) as *const Object) }
}

fn is_object(p: NonNull<Header>) -> bool {
    (unsafe { p.as_ref() }.vt.name)() == std::any::type_name::<Object>()
}

/// A box's kind.
fn kind_of(s: &mut State, p: NonNull<Header>) -> u32 {
    let h = unsafe { p.as_ref() };
    let vt = h.vt as *const _ as usize;
    if is_object(p) {
        let o = object_of(p);
        if let Some(c) = o.shape_ref().class() {
            let key = (vt, c as *const _ as usize);
            if let Some(&k) = s.kind_cache.get(&key) {
                return k;
            }
            let k = intern(s, c.name.to_string());
            s.kind_cache.insert(key, k);
            return k;
        }
        let keys = &o.shape_ref().keys;
        let mut name = String::from("{");
        for (i, k) in keys.iter().take(4).enumerate() {
            if i > 0 {
                name.push(',');
            }
            name.push_str(k);
        }
        if keys.len() > 4 {
            name.push_str(",…");
        }
        name.push('}');
        return intern(s, name);
    }
    let key = (vt, 0);
    if let Some(&k) = s.kind_cache.get(&key) {
        return k;
    }
    let full = (h.vt.name)();
    let short = full.rsplit("::").next().unwrap_or(full).trim_end_matches('>');
    let name = match short {
        "MapObj" => "Map".into(),
        "SetObj" => "Set".into(),
        "Closure" => "Function".into(),
        "Value" if full.contains("RefCell") => "Cell".into(),
        _ => short.to_string(),
    };
    let k = intern(s, name);
    s.kind_cache.insert(key, k);
    k
}

fn is_container(s: &State, k: u32) -> bool {
    matches!(s.kinds[k as usize].as_str(), "Array" | "Map" | "Set")
}

// ───────────── birth and death ─────────────

/// A heap box was allocated (not an arena box).
pub fn born(p: NonNull<Header>, bytes: usize) {
    let s = st();
    let kind = kind_of(s, p);
    s.boxes.insert(p.as_ptr() as usize, Info { kind, bytes: bytes as u32, uf: [NONE; RULES] });
    s.live_bytes += bytes as u64;
    let ks = &mut s.kstats[kind as usize];
    ks.allocated += 1;
    ks.bytes += bytes as u64;
}

/// A heap box's value was dropped (the box may be freed later).
pub fn freed(p: NonNull<Header>) {
    let s = st();
    let Some(i) = s.boxes.remove(&(p.as_ptr() as usize)) else { return };
    s.live_bytes -= i.bytes as u64;
    if i.uf == [NONE; RULES] && s.cyclic[i.kind as usize] {
        s.unseen_cyclic += 1;
        s.unseen_cyclic_bytes += i.bytes as u64;
    }
    for r in 0..RULES {
        if i.uf[r] == NONE {
            continue;
        }
        let rs = &mut s.rules[r];
        let root = rs.find(i.uf[r]) as usize;
        let g = &mut rs.regions[root];
        g.live -= 1;
        g.live_bytes -= i.bytes as u64;
        g.dead_bytes += i.bytes as u64;
        if g.live == 0 {
            // No member left: the region was freed whole (at the latest).
            g.dead_bytes = 0;
            s.region_frees[r] += 1;
        }
    }
}

// ───────────── snapshots ─────────────

/// Whether a snapshot is due (checked at safepoints).
#[inline(always)]
pub fn due() -> bool {
    on() && STATS.allocated.get() >= st().next
}

/// A box's children (arena boxes and acyclic leaves left out).
fn children(p: NonNull<Header>, out: &mut Vec<usize>) {
    out.clear();
    let mut visit = |c: NonNull<Header>| {
        let h = unsafe { c.as_ref() };
        if !h.has(flag::ACYCLIC) && !h.has(flag::ARENA) {
            out.push(c.as_ptr() as usize);
        }
    };
    trace_header(p, &mut Tracer { visit: &mut visit, shared: false });
}

/// The heap as a graph over registered boxes, by index.
struct Graph {
    addrs: Vec<usize>,
    kind: Vec<u32>,
    bytes: Vec<u64>,
    kids: Vec<Vec<u32>>,
}

/// Strongly connected components of `adj` (iterative Tarjan): the
/// component of each node, and whether it has an edge inside it.
fn scc(adj: &[Vec<u32>]) -> (Vec<u32>, Vec<bool>) {
    let n = adj.len();
    let mut index = vec![u32::MAX; n];
    let mut low = vec![0u32; n];
    let mut on = vec![false; n];
    let mut comp = vec![u32::MAX; n];
    let mut cyclic = Vec::new();
    let mut stack = Vec::new();
    let mut next = 0u32;
    for root in 0..n {
        if index[root] != u32::MAX {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(top) = call.last_mut() {
            let v = top.0;
            if top.1 < adj[v].len() {
                let w = adj[v][top.1] as usize;
                top.1 += 1;
                if index[w] == u32::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    call.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                call.pop();
                if let Some(&(u, _)) = call.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] == index[v] {
                    let c = cyclic.len() as u32;
                    let mut size = 0;
                    loop {
                        let w = stack.pop().unwrap();
                        on[w] = false;
                        comp[w] = c;
                        size += 1;
                        if w == v {
                            break;
                        }
                    }
                    cyclic.push(size > 1 || adj[v].contains(&(v as u32)));
                }
            }
        }
    }
    (comp, cyclic)
}

/// Trace the heap: update the kind graph and the regions, and measure.
pub fn snapshot() {
    let s = st();
    let at_alloc = STATS.allocated.get();
    let addrs: Vec<usize> = s.boxes.keys().copied().collect();
    let index: FxHashMap<usize, u32> = addrs.iter().enumerate().map(|(i, &a)| (a, i as u32)).collect();
    let mut g = Graph { kind: Vec::with_capacity(addrs.len()), bytes: Vec::with_capacity(addrs.len()), kids: Vec::with_capacity(addrs.len()), addrs };
    let mut buf = Vec::new();
    for &a in &g.addrs {
        let i = s.boxes[&a];
        g.kind.push(i.kind);
        g.bytes.push(i.bytes as u64);
        children(NonNull::new(a as *mut Header).unwrap(), &mut buf);
        g.kids.push(buf.iter().filter_map(|c| index.get(c).copied()).collect());
    }
    let n = g.addrs.len();
    let container: Vec<bool> = g.kind.iter().map(|&k| is_container(s, k)).collect();

    // Kind graph: containers transparent (their element kinds, through
    // nested containers).
    let mut elems: Vec<Option<Vec<u32>>> = vec![None; n];
    fn elem_kinds(g: &Graph, container: &[bool], memo: &mut Vec<Option<Vec<u32>>>, a: usize, depth: u32) -> Vec<u32> {
        if let Some(v) = &memo[a] {
            return v.clone();
        }
        memo[a] = Some(Vec::new()); // cut cycles among containers
        let mut out = Vec::new();
        for &c in &g.kids[a] {
            if container[c as usize] && depth < 8 {
                out.extend(elem_kinds(g, container, memo, c as usize, depth + 1));
            } else {
                out.push(g.kind[c as usize]);
            }
        }
        out.sort_unstable();
        out.dedup();
        memo[a] = Some(out.clone());
        out
    }
    for a in 0..n {
        if container[a] {
            continue;
        }
        for &c in &g.kids[a] {
            if container[c as usize] {
                for e in elem_kinds(&g, &container, &mut elems, c as usize, 0) {
                    s.kedges.insert((g.kind[a], e));
                }
            } else {
                s.kedges.insert((g.kind[a], g.kind[c as usize]));
            }
        }
    }
    let mut kadj = vec![Vec::new(); s.kinds.len()];
    for &(a, b) in &s.kedges {
        kadj[a as usize].push(b);
    }
    let (kcomp, kcyc) = scc(&kadj);
    for k in 0..s.kinds.len() {
        s.cyclic[k] = kcyc[kcomp[k] as usize];
    }

    // Region members: boxes of cyclic kinds, and containers holding them.
    let relevant: Vec<bool> = (0..n)
        .map(|a| s.cyclic[g.kind[a] as usize] || (container[a] && elem_kinds(&g, &container, &mut elems, a, 0).iter().any(|&e| s.cyclic[e as usize])))
        .collect();

    // Reachability: roots are boxes referenced from outside the heap.
    let mut indeg = vec![0u32; n];
    for kids in &g.kids {
        for &c in kids {
            indeg[c as usize] += 1;
        }
    }
    let mut reach = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    for a in 0..n {
        let rc = unsafe { (*(g.addrs[a] as *const Header)).rc.get() };
        if rc > indeg[a] {
            reach[a] = true;
            stack.push(a as u32);
        }
    }
    while let Some(a) = stack.pop() {
        for &c in &g.kids[a as usize] {
            if !reach[c as usize] {
                reach[c as usize] = true;
                stack.push(c);
            }
        }
    }
    let reachable: u64 = (0..n).filter(|&a| reach[a]).map(|a| g.bytes[a]).sum();

    // Region rules.
    let mut snaps = [RuleSnap::default(); RULES];
    for (r, snap) in snaps.iter_mut().enumerate() {
        // Each box's union-find node (fresh for boxes new to regions).
        let mut uf: Vec<u32> = (0..n).map(|a| s.boxes[&g.addrs[a]].uf[r]).collect();
        let rs = &mut s.rules[r];
        if r == 0 {
            for a in 0..n {
                if !relevant[a] {
                    continue;
                }
                if uf[a] == NONE {
                    uf[a] = rs.fresh(g.bytes[a]);
                }
                for &c in &g.kids[a] {
                    let c = c as usize;
                    if relevant[c] {
                        if uf[c] == NONE {
                            uf[c] = rs.fresh(g.bytes[c]);
                        }
                        rs.union(uf[a], uf[c]);
                    }
                }
            }
        } else {
            // New boxes join the region of what refers to them: first from
            // boxes already in regions, then from new structures' entries
            // (boxes no other new box refers to), then from what's left
            // (new cycles).
            let join = |rs: &mut Regions, uf: &mut Vec<u32>, from: Vec<u32>| {
                let mut stack = from;
                while let Some(a) = stack.pop() {
                    for &c in &g.kids[a as usize] {
                        let c = c as usize;
                        if relevant[c] && uf[c] == NONE {
                            uf[c] = rs.fresh(g.bytes[c]);
                            rs.union(uf[a as usize], uf[c]);
                            stack.push(c as u32);
                        }
                    }
                }
            };
            let old: Vec<u32> = (0..n).filter(|&a| relevant[a] && uf[a] != NONE).map(|a| a as u32).collect();
            join(rs, &mut uf, old);
            let mut newdeg = vec![0u32; n];
            for a in 0..n {
                if relevant[a] && uf[a] == NONE {
                    for &c in &g.kids[a] {
                        newdeg[c as usize] += 1;
                    }
                }
            }
            let entries: Vec<usize> = (0..n).filter(|&a| relevant[a] && uf[a] == NONE && newdeg[a] == 0).collect();
            let rest: Vec<usize> = (0..n).filter(|&a| relevant[a] && uf[a] == NONE && newdeg[a] > 0).collect();
            for a in entries.into_iter().chain(rest) {
                if uf[a] == NONE {
                    uf[a] = rs.fresh(g.bytes[a]);
                    join(rs, &mut uf, vec![a as u32]);
                }
            }
            // Merge regions whose references form a cycle.
            let mut rid: FxHashMap<u32, u32> = FxHashMap::default();
            let mut roots: Vec<u32> = Vec::new();
            for a in 0..n {
                if relevant[a] {
                    let root = rs.find(uf[a]);
                    rid.entry(root).or_insert_with(|| {
                        roots.push(root);
                        roots.len() as u32 - 1
                    });
                }
            }
            let mut radj: Vec<Vec<u32>> = vec![Vec::new(); roots.len()];
            for a in 0..n {
                if !relevant[a] {
                    continue;
                }
                let ra = rid[&rs.find(uf[a])];
                for &c in &g.kids[a] {
                    if relevant[c as usize] {
                        let rc = rid[&rs.find(uf[c as usize])];
                        if rc != ra {
                            radj[ra as usize].push(rc);
                        }
                    }
                }
            }
            let (comp, _) = scc(&radj);
            let mut first: FxHashMap<u32, u32> = FxHashMap::default();
            for (i, &c) in comp.iter().enumerate() {
                match first.get(&c) {
                    Some(&f) => {
                        rs.union(f, roots[i]);
                    }
                    None => {
                        first.insert(c, roots[i]);
                    }
                }
            }
        }
        for a in 0..n {
            if uf[a] != NONE {
                s.boxes.get_mut(&g.addrs[a]).unwrap().uf[r] = uf[a];
            }
        }
        // Measure: regions with a reachable member are held whole.
        let rs = &mut s.rules[r];
        let mut held: FxHashMap<u32, (bool, u64)> = FxHashMap::default();
        for a in 0..n {
            if uf[a] == NONE {
                continue;
            }
            let e = held.entry(rs.find(uf[a])).or_default();
            e.0 |= reach[a];
            if !reach[a] {
                e.1 += g.bytes[a];
            }
        }
        let mut floating = 0;
        for (&root, &(live, unreachable)) in &held {
            if !live {
                continue;
            }
            let reg = rs.regions[root as usize];
            floating += reg.dead_bytes + unreachable;
            snap.regions += 1;
            if reg.live_bytes > snap.largest {
                snap.largest = reg.live_bytes;
                snap.largest_members = reg.live;
            }
        }
        snap.floating = floating;
        snap.design = reachable + floating;
    }
    s.snaps.push(Snap { at_alloc, live: s.live_bytes, reachable, rules: snaps });
    let every = s.every.unwrap_or((s.boxes.len() as u64 / 2).max(10_000));
    s.next = STATS.allocated.get() + every;
}

// ───────────── cycle collections ─────────────

/// A collection is about to trace from `cands`: count them by kind, and
/// how many boxes tracing from those of cyclic kinds alone would reach.
pub fn before_collect(cands: &[NonNull<Header>]) -> (u64, u64) {
    let s = st();
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    let mut stack = Vec::new();
    let mut ncyc = 0;
    let mut buf = Vec::new();
    for &p in cands {
        let Some(k) = s.boxes.get(&(p.as_ptr() as usize)).map(|i| i.kind) else { continue };
        s.kstats[k as usize].candidates += 1;
        if !s.cyclic[k as usize] {
            continue;
        }
        ncyc += 1;
        if seen.insert(p.as_ptr() as usize) {
            stack.push(p.as_ptr() as usize);
        }
        while let Some(a) = stack.pop() {
            children(NonNull::new(a as *mut Header).unwrap(), &mut buf);
            for &c in &buf {
                if seen.insert(c) {
                    stack.push(c);
                }
            }
        }
    }
    (ncyc, seen.len() as u64)
}

/// A collection ended.
pub fn after_collect(candidates: usize, traced: usize, garbage: &[NonNull<Header>], ns: u64, est: (u64, u64)) {
    let s = st();
    for &p in garbage {
        if let Some(i) = s.boxes.get(&(p.as_ptr() as usize)) {
            s.kstats[i.kind as usize].garbage += 1;
        }
    }
    s.collections.push(Collection {
        at_alloc: STATS.allocated.get(),
        candidates: candidates as u64,
        traced: traced as u64,
        garbage: garbage.len() as u64,
        ns,
        cyclic_candidates: est.0,
        traced_cyclic_only: est.1,
    });
}

// ───────────── report ─────────────

pub fn finish() {
    if !on() {
        return;
    }
    snapshot();
    ON.store(false, Relaxed);
    let s = st();
    let mb = |b: f64| b / 1048576.0;
    let sep = |n: usize| if n > 0 { ",\n" } else { "\n" };
    let mut j = String::new();
    j.push_str(&format!("{{\"rules\": {RULE_NAMES:?},\n\"kinds\": ["));
    let mut order: Vec<usize> = (0..s.kinds.len()).collect();
    order.sort_by_key(|&k| std::cmp::Reverse(s.kstats[k].bytes));
    for (n, &k) in order.iter().enumerate() {
        let ks = &s.kstats[k];
        j.push_str(&format!(
            "{}{{\"kind\": {:?}, \"cyclic\": {}, \"allocated\": {}, \"bytes\": {}, \"candidates\": {}, \"garbage\": {}}}",
            sep(n), s.kinds[k], s.cyclic[k], ks.allocated, ks.bytes, ks.candidates, ks.garbage
        ));
    }
    j.push_str("],\n\"kind_edges\": [");
    for (n, &(a, b)) in s.kedges.iter().enumerate() {
        j.push_str(&format!("{}[{:?},{:?}]", if n > 0 { "," } else { "" }, s.kinds[a as usize], s.kinds[b as usize]));
    }
    // [at_alloc, candidates, traced, garbage, ns, cyclic_candidates, traced_cyclic_only]
    j.push_str("],\n\"collections\": [");
    for (n, c) in s.collections.iter().enumerate() {
        j.push_str(&format!("{}[{},{},{},{},{},{},{}]", sep(n), c.at_alloc, c.candidates, c.traced, c.garbage, c.ns, c.cyclic_candidates, c.traced_cyclic_only));
    }
    // [at_alloc, live, reachable, then per rule: design, floating, largest, largest_members, regions]
    j.push_str("],\n\"snapshots\": [");
    for (n, x) in s.snaps.iter().enumerate() {
        j.push_str(&format!("{}[{},{},{}", sep(n), x.at_alloc, x.live, x.reachable));
        for r in &x.rules {
            j.push_str(&format!(",{},{},{},{},{}", r.design, r.floating, r.largest, r.largest_members, r.regions));
        }
        j.push(']');
    }
    j.push_str(&format!(
        "],\n\"region_frees\": {:?}, \"unseen_cyclic\": {}, \"unseen_cyclic_bytes\": {}}}\n",
        s.region_frees, s.unseen_cyclic, s.unseen_cyclic_bytes
    ));
    if let Err(e) = std::fs::write(&s.path, j) {
        eprintln!("rts: RTS_CENSUS {}: {e}", s.path);
    }

    let tot: u64 = s.kstats.iter().map(|k| k.bytes).sum();
    let cyc: u64 = (0..s.kinds.len()).filter(|&k| s.cyclic[k]).map(|k| s.kstats[k].bytes).sum();
    let traced: u64 = s.collections.iter().map(|c| c.traced).sum();
    let traced_c: u64 = s.collections.iter().map(|c| c.traced_cyclic_only).sum();
    let garbage: u64 = s.collections.iter().map(|c| c.garbage).sum();
    let cyclic: Vec<&str> = (0..s.kinds.len()).filter(|&k| s.cyclic[k]).map(|k| s.kinds[k].as_str()).collect();
    eprintln!("census: cyclic kinds: {}", cyclic.join(" "));
    eprintln!("  allocated in cyclic kinds  {:.1}% of {:.1} MB", 100.0 * cyc as f64 / tot.max(1) as f64, mb(tot as f64));
    eprintln!(
        "  cycle collections {}: traced {traced}, {:.1}% of it from cyclic-kind candidates; {garbage} garbage",
        s.collections.len(),
        100.0 * traced_c as f64 / traced.max(1) as f64
    );
    let n = s.snaps.len().max(1) as f64;
    let mean = |f: &dyn Fn(&Snap) -> u64| mb(s.snaps.iter().map(|x| f(x) as f64).sum::<f64>() / n);
    let max = |f: &dyn Fn(&Snap) -> u64| mb(s.snaps.iter().map(f).max().unwrap_or(0) as f64);
    eprintln!("  {} snapshots, MB mean / max:", s.snaps.len());
    eprintln!("    this build (RC + cycle collector) {:8.2} {:8.2}", mean(&|x| x.live), max(&|x| x.live));
    eprintln!("    exact (reachable)                 {:8.2} {:8.2}", mean(&|x| x.reachable), max(&|x| x.reachable));
    for r in 0..RULES {
        eprintln!(
            "    regions, {:5} rule                {:8.2} {:8.2}   floating max {:.2}, largest region {:.2} MB, {} region frees",
            RULE_NAMES[r],
            mean(&|x| x.rules[r].design),
            max(&|x| x.rules[r].design),
            max(&|x| x.rules[r].floating),
            max(&|x| x.rules[r].largest),
            s.region_frees[r]
        );
    }
    eprintln!("  cyclic-kind boxes freed before any snapshot: {} ({:.2} MB)", s.unseen_cyclic, mb(s.unseen_cyclic_bytes as f64));
}
