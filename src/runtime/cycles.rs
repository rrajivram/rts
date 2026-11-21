//! Cycle collection for the reference-counting build (Bacon & Rajan,
//! "Concurrent Cycle Collection in Reference Counted Systems", ECOOP 2001,
//! synchronous variant).
//!
//! Reference counting frees everything except garbage cycles. A box whose
//! count drops to a non-zero value *might* have just become the entry point
//! of a garbage cycle, so it is remembered as a candidate ("possible root").
//! When enough candidates accumulate, the VM calls `collect` at a safepoint:
//!
//! 1. **Mark gray**: from each candidate, subtract the references that come
//!    from inside the candidate's subgraph (trial deletion).
//! 2. **Scan**: a box whose count is still positive is referenced from outside,
//!    so it and everything it reaches are live again (counts restored).
//!    The rest are white: referenced only from within garbage cycles.
//! 3. **Collect white**: free the white boxes.
//!
//! Boxes flagged `ACYCLIC` (strings, number arrays, …: values that can never
//! hold a `Gc` pointer) are never candidates and are skipped while tracing.
//! Arena boxes never become candidates either; their huge count keeps them,
//! and whatever they reach, live during a collection.

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::time::Instant;

use super::heap::{flag, free_box, trace_header, Header, Tracer, STATS};

const BLACK: u32 = 0;
const GRAY: u32 = 1;
const WHITE: u32 = 2;
const PURPLE: u32 = 3;

struct State {
    roots: Vec<NonNull<Header>>,
    /// Collect when this many candidates are buffered.
    threshold: usize,
    /// The threshold was set explicitly: don't adapt it.
    fixed: bool,
    /// Owners build: the buffer's first `kept` entries are compacted.
    #[cfg_attr(not(feature = "owners"), allow(dead_code))]
    kept: usize,
    enabled: bool,
}

struct Global(UnsafeCell<State>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State {
    roots: Vec::new(),
    threshold: 10_000,
    fixed: false,
    kept: 0,
    enabled: true,
}));

#[allow(clippy::mut_from_ref)]
fn st() -> &'static mut State {
    unsafe { &mut *STATE.0.get() }
}

#[inline]
fn color(h: &Header) -> u32 {
    (h.flags.get() & flag::COLOR_MASK) >> flag::COLOR_SHIFT
}

#[inline]
fn set_color(h: &Header, c: u32) {
    h.flags.set((h.flags.get() & !flag::COLOR_MASK) | (c << flag::COLOR_SHIFT));
}

/// Collect when `n` candidates are buffered (`RTS_CC_THRESHOLD`, for testing).
pub fn set_threshold(n: usize) {
    st().threshold = n.max(1);
    st().fixed = true;
}

/// Turn cycle collection off (`--no-cycles`): measures what it costs.
pub fn disable() {
    st().enabled = false;
}

/// A count dropped to a non-zero value: remember the box as a candidate.
#[inline(never)]
pub fn possible_root(p: NonNull<Header>) {
    let s = st();
    if !s.enabled {
        return;
    }
    let h = unsafe { p.as_ref() };
    // A member of the check under way (the check's barrier): live. Keep
    // its colour, which is the check's.
    #[cfg(feature = "owners")]
    if h.has(flag::MEMBER) {
        h.set(flag::TOUCHED);
        if h.flags.get() & super::heap::QUIET == 0 {
            h.set(flag::BUFFERED);
            s.roots.push(p);
        }
        return;
    }
    set_color(h, PURPLE);
    h.set(flag::BUFFERED);
    s.roots.push(p);
}

/// Whether enough candidates are buffered to be worth a collection.
#[inline(always)]
pub fn wanted() -> bool {
    ready() || super::census::due()
}

/// Whether enough candidates are buffered (without the census). With
/// owners, also whenever the heap grew by `GROWTH` since the last check
/// and anything is buffered: a candidate can hold a large structure.
#[inline(always)]
pub fn ready() -> bool {
    let s = st();
    #[cfg(feature = "owners")]
    {
        if checking() {
            return total_bytes() >= SLICE_AT.load(std::sync::atomic::Ordering::Relaxed);
        }
        s.roots.len() >= s.kept + compact_every(s) || grown()
    }
    #[cfg(not(feature = "owners"))]
    {
        s.roots.len() >= s.threshold
    }
}

/// Owners build: check when live bytes reach this.
static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
/// Owners build: heap growth between checks while candidates wait: a
/// quarter of the program's heap, at least this.
const GROWTH: usize = 512 << 10;

/// The program starts (owners build): the growth trigger counts from here.
pub fn start_run() {
    NEXT.store(super::heap::LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed) + GROWTH, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "owners")]
    if let Some(us) = std::env::var("RTS_CC_SLICE_US").ok().and_then(|v| v.parse().ok()) {
        SLICE_US.store(us, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(feature = "owners")]
    if let Some(n) = std::env::var("RTS_CC_BYTES_PER_STEP").ok().and_then(|v| v.parse().ok()) {
        BYTES_PER_STEP.store(n, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Visit the children of a box, skipping acyclic ones.
#[inline]
fn each_child(p: NonNull<Header>, mut f: impl FnMut(NonNull<Header>)) {
    let mut visit = |c: NonNull<Header>| {
        if !unsafe { c.as_ref() }.has(flag::ACYCLIC) {
            f(c);
        }
    };
    trace_header(p, &mut Tracer { visit: &mut visit, shared: false });
}

/// Run a collection. Call only at a safepoint: no `RefCell` of a heap value
/// may be mutably borrowed (those that are, are treated as opaque, which is
/// safe: their children just look externally referenced).
///
/// `uncounted`: sorted addresses that refer to boxes without counting
/// (deferred reference counting's stack words; empty otherwise).
pub fn collect(uncounted: &[usize]) {
    #[cfg(feature = "owners")]
    if uncounted.is_empty() {
        return collect_owned();
    }
    let start = Instant::now();
    let s = st();
    let roots = std::mem::take(&mut s.roots);
    let mut stack: Vec<NonNull<Header>> = Vec::new();

    // 1. Mark gray from each live purple candidate; drop the others.
    let mut live_roots = Vec::with_capacity(roots.len());
    for p in roots {
        let h = unsafe { p.as_ref() };
        if h.has(flag::DEAD) {
            // Its count reached zero while buffered: only the box is left.
            free_box(p);
            continue;
        }
        if color(h) == PURPLE && h.rc.get() > 0 {
            live_roots.push(p);
        } else {
            h.clear(flag::BUFFERED);
        }
    }
    // Purging dead and no-longer-purple entries is cheap; trial deletion is
    // not. If few live candidates remain, keep them buffered and wait.
    if !s.fixed && live_roots.len() < s.threshold / 2 {
        s.roots = live_roots;
        STATS.pause(start.elapsed(), super::memtrace::Kind::Cycles);
        return;
    }
    let census = if super::census::on() { Some(super::census::before_collect(&live_roots)) } else { None };
    let mut traced = 0usize;
    for &p in &live_roots {
        let h = unsafe { p.as_ref() };
        if color(h) == GRAY {
            continue;
        }
        set_color(h, GRAY);
        stack.push(p);
        while let Some(n) = stack.pop() {
            traced += 1;
            each_child(n, |c| {
                let ch = unsafe { c.as_ref() };
                ch.rc.set(ch.rc.get() - 1);
                if color(ch) != GRAY {
                    set_color(ch, GRAY);
                    stack.push(c);
                }
            });
        }
    }

    // 2. Scan: externally referenced boxes (and what they reach) go black.
    let mut black: Vec<NonNull<Header>> = Vec::new();
    for &p in &live_roots {
        stack.push(p);
        while let Some(n) = stack.pop() {
            let h = unsafe { n.as_ref() };
            if color(h) != GRAY {
                continue;
            }
            if h.rc.get() > 0 || (!uncounted.is_empty() && super::stack::refers(uncounted, n.as_ptr() as usize)) {
                set_color(h, BLACK);
                black.push(n);
                while let Some(m) = black.pop() {
                    each_child(m, |c| {
                        let ch = unsafe { c.as_ref() };
                        ch.rc.set(ch.rc.get() + 1);
                        if color(ch) != BLACK {
                            set_color(ch, BLACK);
                            black.push(c);
                        }
                    });
                }
            } else {
                set_color(h, WHITE);
                each_child(n, |c| stack.push(c));
            }
        }
    }

    // 3. Gather the white boxes.
    let mut garbage: Vec<NonNull<Header>> = Vec::new();
    for &p in &live_roots {
        unsafe { p.as_ref() }.clear(flag::BUFFERED);
    }
    for &p in &live_roots {
        stack.push(p);
        while let Some(n) = stack.pop() {
            let h = unsafe { n.as_ref() };
            if color(h) == WHITE && !h.has(flag::BUFFERED) {
                set_color(h, BLACK);
                h.set(flag::MARK); // "in the garbage set", until freed
                garbage.push(n);
                each_child(n, |c| stack.push(c));
            }
        }
    }

    // Trial deletion left every edge from a garbage box to a live box
    // subtracted. The garbage values are about to be dropped normally, which
    // subtracts those edges again, so restore them first.
    for &p in &garbage {
        each_child(p, |c| {
            let ch = unsafe { c.as_ref() };
            if !ch.has(flag::MARK) {
                ch.rc.set(ch.rc.get() + 1);
            }
        });
    }
    // Free them: first pin every white box (so the references between them,
    // released while their values drop, can't free a box twice), then drop
    // all values, then free all boxes.
    if let Some(est) = census {
        super::census::after_collect(live_roots.len(), traced, &garbage, start.elapsed().as_nanos() as u64, est);
    }
    for &p in &garbage {
        let h = unsafe { p.as_ref() };
        h.rc.set(u32::MAX / 2);
        h.set(flag::ARENA); // not a candidate while its value drops
    }
    for &p in &garbage {
        unsafe { super::heap::drop_header_value(p) };
    }
    for &p in &garbage {
        free_box(p);
    }
    STATS.collected.set(STATS.collected.get() + garbage.len() as u64);

    // Collect less often if most candidates were live.
    if !s.fixed {
        s.threshold = if garbage.len() * 4 < live_roots.len() { (s.threshold * 2).min(1 << 18) } else { s.threshold.max(10_000) };
    }
    STATS.pause(start.elapsed(), super::memtrace::Kind::Cycles);
}

/// Ownership's check (`owners` build): candidates are boxes without an
/// owner whose count dropped to a non-zero value. Trial deletion runs over
/// what the candidates own (owner links down) and the other unowned roots
/// they borrow, not over everything they reach: a garbage structure is found
/// when its root loses its last outside reference, at a cost proportional
/// to what the root owns.
///
/// The check is incremental: it runs in slices of bounded time at
/// safepoints, the program running in between, so no single pause is
/// longer than a slice (and one linear pass over the members, below).
///
/// 1. Gather: the members, flagged `MEMBER` (a heuristic set: missing a
///    box only makes its referents look referenced from outside).
/// 2. Trial: count in each member the references it has from members (in
///    three bits of its flags, a side table beyond; counts themselves are
///    left alone, so the program keeps counting normally).
/// 3. Scan: a member counted more than that is referenced from outside, so
///    it and the members it reaches are live (black).
/// 4. Verify, in one go: the same over every member still gray. A member
///    whose count dropped since the check began is `TOUCHED` (the barrier:
///    `heap::noisy` sends a member's decrement to `possible_root`), and is
///    live too. What is still gray is garbage: every reference to it is one
///    counted in 2 that still exists (none was removed, or it would be
///    touched; none was added, or its count would be higher), from a gray
///    member (a black one would have reached it).
/// 5. Collect, clear, drop, free: in slices (garbage can't change).
///
/// A member whose count reaches zero during the check keeps its box until
/// the check ends (`DEAD`).
///
/// Slices are paced by allocation (`BYTES_PER_STEP`): the program can't
/// make garbage faster than the check gets through it.
///
/// Survivors: an unowned box a check found live is a `SURVIVOR`, and
/// remembers how many references it had from members. When its count drops
/// again it is buffered as usual, but while the count is still above that
/// number it is most likely still referenced from outside, so an ordinary
/// check leaves it (and whatever it reaches) out. Every survivor is on the
/// full check's list (`HELD`): a full check, which runs when the heap has
/// grown by an eighth since the last one, looks at all of them. Without
/// this, a large live structure held from the stack would be traced again
/// at every check (a quarter of Havlak's run time). `RTS_CC_FULL`: every
/// check is full (less memory, more time).
///
/// The candidate buffer is compacted every `COMPACT_EVERY` new entries (a
/// box whose count reaches zero while buffered keeps its memory until
/// then); a check starts when enough candidates are left.
#[cfg(feature = "owners")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Sort,
    Gather,
    Trial,
    Scan,
    Collect,
    Clear,
    Drop,
    Free,
}

#[cfg(feature = "owners")]
struct Check {
    phase: Phase,
    /// The members; the first `roots` are the candidates.
    members: Vec<NonNull<Header>>,
    roots: usize,
    full: bool,
    /// Propagation met a borrowed cell it couldn't see into: free nothing.
    abort: bool,
    /// Position in `members` (or `garbage`) in the current phase.
    i: usize,
    stack: Vec<NonNull<Header>>,
    garbage: Vec<NonNull<Header>>,
    /// Survivors an ordinary check left to the full check (flagged `HELD`).
    old: Vec<NonNull<Header>>,
    /// Candidates an ordinary check waits for.
    threshold: usize,
    /// Members' references from members beyond what their flags hold.
    excess: rustc_hash::FxHashMap<usize, u32>,
    slices: u32,
    work: std::time::Duration,
    /// Time per phase (`RTS_CC_LOG`), and the longest slice.
    phase_ns: [u64; 9],
    /// The candidates being sorted: from the buffer (flagged `BUFFERED`),
    /// then from `old` from `split` on (flagged `HELD`).
    buffer: Vec<NonNull<Header>>,
    split: usize,
    longest: std::time::Duration,
    census: Option<(u64, u64)>,
}

#[cfg(feature = "owners")]
struct CheckCell(UnsafeCell<Check>);
#[cfg(feature = "owners")]
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for CheckCell {}

#[cfg(feature = "owners")]
static CHECK: CheckCell = CheckCell(UnsafeCell::new(Check {
    phase: Phase::Idle,
    members: Vec::new(),
    roots: 0,
    full: false,
    abort: false,
    i: 0,
    stack: Vec::new(),
    garbage: Vec::new(),
    old: Vec::new(),
    threshold: 1_000,
    excess: rustc_hash::FxHashMap::with_hasher(rustc_hash::FxBuildHasher),
    slices: 0,
    work: std::time::Duration::ZERO,
    phase_ns: [0; 9],
    buffer: Vec::new(),
    split: 0,
    longest: std::time::Duration::ZERO,
    census: None,
}));

#[cfg(feature = "owners")]
#[allow(clippy::mut_from_ref)]
fn check() -> &'static mut Check {
    unsafe { &mut *CHECK.0.get() }
}

/// Owners build: a check is under way.
#[cfg(feature = "owners")]
#[inline(always)]
fn checking() -> bool {
    check().phase != Phase::Idle
}

/// Owners build: while a check is under way, run its next slice once the
/// program has allocated this many more bytes (`u64::MAX`: no check).
#[cfg(feature = "owners")]
static SLICE_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// Compact the buffer after this many new entries.
#[cfg(feature = "owners")]
const COMPACT_EVERY: usize = 10_000;
/// Candidates (after compaction) an ordinary check waits for: adapted
/// between these, doubling after a check that found little.
#[cfg(feature = "owners")]
const MIN_THRESHOLD: usize = 256;
#[cfg(feature = "owners")]
const MAX_THRESHOLD: usize = 16_384;
/// Bytes the program may allocate between slices: this many per step of
/// the check's work in the last slice (a step visits a box; a garbage box
/// takes about eight), at least `SLICE_GATE`.
/// (`RTS_CC_BYTES_PER_STEP`: more lets the program run more between slices
/// and leaves garbage longer.)
#[cfg(feature = "owners")]
static BYTES_PER_STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(4);
#[cfg(feature = "owners")]
const SLICE_GATE: u64 = 16 << 10;
/// Longest slice (`RTS_CC_SLICE_US`; 0: each check in one go).
#[cfg(feature = "owners")]
static SLICE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(500);

#[cfg(feature = "owners")]
fn total_bytes() -> u64 {
    super::heap::TOTAL_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(feature = "owners")]
fn live_bytes() -> usize {
    super::heap::LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stops a slice when its time is up (looking at the clock every 64 steps).
#[cfg(feature = "owners")]
struct Budget {
    until: Option<Instant>,
    n: u32,
}

#[cfg(feature = "owners")]
impl Budget {
    #[inline]
    fn out(&mut self) -> bool {
        self.n = self.n.wrapping_add(1);
        self.n & 63 == 0 && self.until.is_some_and(|u| Instant::now() >= u)
    }
}

/// A member's references from members (see `flag::INDEG_SHIFT`).
#[cfg(feature = "owners")]
#[inline]
fn indeg(h: &Header) -> u32 {
    let d = (h.flags.get() & flag::INDEG_MASK) >> flag::INDEG_SHIFT;
    if d < flag::INDEG_MAX {
        d
    } else {
        d + check().excess.get(&(h as *const Header as usize)).copied().unwrap_or(0)
    }
}

/// A survivor's references from members at the check that found it live.
#[cfg(feature = "owners")]
#[inline]
fn stored(h: &Header) -> u32 {
    (h.flags.get() & flag::INDEG_MASK) >> flag::INDEG_SHIFT
}

#[cfg(feature = "owners")]
#[inline]
fn bump(h: &Header) {
    let d = (h.flags.get() & flag::INDEG_MASK) >> flag::INDEG_SHIFT;
    if d < flag::INDEG_MAX {
        h.flags.set(h.flags.get() + (1 << flag::INDEG_SHIFT));
    } else {
        *check().excess.entry(h as *const Header as usize).or_insert(0) += 1;
    }
}

/// Live by the check's evidence: touched, or counted beyond its references
/// from members.
#[cfg(feature = "owners")]
#[inline]
fn seed(h: &Header) -> bool {
    color(h) == GRAY && !h.has(flag::DEAD) && (h.has(flag::TOUCHED) || h.rc.get() > indeg(h))
}

#[cfg(feature = "owners")]
#[inline]
fn join(members: &mut Vec<NonNull<Header>>, p: NonNull<Header>) {
    let h = unsafe { p.as_ref() };
    h.flags.set((h.flags.get() & !(flag::TOUCHED | flag::INDEG_MASK | flag::COLOR_MASK)) | flag::MEMBER | (GRAY << flag::COLOR_SHIFT));
    members.push(p);
}

/// Owners build: compact the buffer after this many new entries.
#[cfg(feature = "owners")]
#[inline(always)]
fn compact_every(s: &State) -> usize {
    if s.fixed { s.threshold } else { COMPACT_EVERY }
}

/// Owners build: the heap grew enough since the last full check, and there
/// is something to check.
#[cfg(feature = "owners")]
#[inline(always)]
fn grown() -> bool {
    let s = st();
    (!s.roots.is_empty() || !check().old.is_empty()) && live_bytes() >= NEXT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Compact the buffer's new entries: free the boxes whose count reached
/// zero there (their memory waits on the buffer), drop the owned ones, and
/// move survivors still referenced from outside to the full check's list.
/// What is left are candidates.
#[cfg(feature = "owners")]
fn compact(c: &mut Check) {
    let s = st();
    let mut w = s.kept;
    for r in s.kept..s.roots.len() {
        let p = s.roots[r];
        let h = unsafe { p.as_ref() };
        if h.has(flag::DEAD) {
            h.clear(flag::BUFFERED);
            if h.flags.get() & (flag::HELD | flag::MEMBER) == 0 {
                free_box(p);
            }
            continue;
        }
        if !h.has(flag::MEMBER) {
            if h.has(flag::OWNED) {
                h.clear(flag::BUFFERED);
                continue;
            }
            if h.has(flag::SURVIVOR) && stored(h) < flag::INDEG_MAX && h.rc.get() > stored(h) {
                h.clear(flag::BUFFERED);
                if !h.has(flag::HELD) {
                    h.set(flag::HELD);
                    c.old.push(p);
                }
                continue;
            }
        }
        s.roots[w] = p;
        w += 1;
    }
    s.roots.truncate(w);
    s.kept = w;
}

/// Begin a check: take the candidate buffer (and, for a full check, the
/// survivors), to sort in slices.
#[cfg(feature = "owners")]
fn begin(c: &mut Check) {
    c.full = super::census::on() || live_bytes() >= NEXT.load(std::sync::atomic::Ordering::Relaxed) || std::env::var_os("RTS_CC_FULL").is_some();
    c.buffer = std::mem::take(&mut st().roots);
    st().kept = 0;
    c.split = c.buffer.len();
    if c.full {
        c.buffer.append(&mut c.old);
    }
    c.members.clear();
    c.abort = false;
    c.i = 0;
    c.slices = 0;
    c.work = std::time::Duration::ZERO;
    c.phase_ns = [0; 9];
    c.longest = std::time::Duration::ZERO;
    c.phase = Phase::Sort;
}

/// Run the check's phases until it ends or the budget is spent; true when
/// it ended.
#[cfg(feature = "owners")]
fn run(c: &mut Check, b: &mut Budget) -> bool {
    let log = std::env::var_os("RTS_CC_LOG").is_some();
    loop {
        let (t0, ph) = (if log { Some(Instant::now()) } else { None }, c.phase);
        let done = run_phase(c, b);
        if let Some(t0) = t0 {
            c.phase_ns[ph as usize] += t0.elapsed().as_nanos() as u64;
        }
        match done {
            Some(true) => return true,
            Some(false) => return false,
            None => {}
        }
    }
}

/// Run the current phase: `Some(false)` if the budget ran out, `Some(true)`
/// if the check ended, `None` when the next phase is due.
#[cfg(feature = "owners")]
fn run_phase(c: &mut Check, b: &mut Budget) -> Option<bool> {
    {
        match c.phase {
            Phase::Idle => Some(true),
            Phase::Sort => {
                while c.i < c.buffer.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let p = c.buffer[c.i];
                    let h = unsafe { p.as_ref() };
                    h.clear(if c.i < c.split { flag::BUFFERED } else { flag::HELD });
                    c.i += 1;
                    if h.has(flag::DEAD) {
                        // The last list to let go of it frees it.
                        if h.flags.get() & (flag::BUFFERED | flag::HELD | flag::MEMBER) == 0 {
                            free_box(p);
                        }
                    } else if h.flags.get() & (flag::MEMBER | flag::OWNED) != 0 {
                        // On both lists, or owned since: its owner keeps it.
                    } else if !c.full && h.has(flag::SURVIVOR) && stored(h) < flag::INDEG_MAX && h.rc.get() > stored(h) {
                        // Still referenced from outside its structure, most
                        // likely: the full check will look.
                        if !h.has(flag::HELD) {
                            h.set(flag::HELD);
                            c.old.push(p);
                        }
                    } else {
                        join(&mut c.members, p);
                    }
                }
                c.buffer = Vec::new();
                c.roots = c.members.len();
                c.census = if super::census::on() { Some(super::census::before_collect(&c.members)) } else { None };
                c.i = 0;
                c.phase = Phase::Gather;
                None
            }
            Phase::Gather => {
                let full = c.full;
                while c.i < c.members.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let m = c.members[c.i];
                    c.i += 1;
                    if unsafe { m.as_ref() }.has(flag::DEAD) {
                        continue;
                    }
                    let a = m.as_ptr() as usize;
                    let (members, old) = (&mut c.members, &mut c.old);
                    each_child(m, |k| {
                        let ch = unsafe { k.as_ref() };
                        if ch.flags.get() & (flag::MEMBER | flag::ARENA) != 0 {
                            return;
                        }
                        let o = ch.owner.get();
                        if o == a || (o == 0 && (full || !ch.has(flag::SURVIVOR))) {
                            join(members, k);
                        } else if o == 0 && !ch.has(flag::HELD) {
                            // A survivor left out: left to the full check.
                            ch.set(flag::HELD);
                            old.push(k);
                        }
                    });
                }
                c.i = 0;
                c.phase = Phase::Trial;
                None
            }
            Phase::Trial => {
                while c.i < c.members.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let m = c.members[c.i];
                    c.i += 1;
                    if unsafe { m.as_ref() }.has(flag::DEAD) {
                        continue;
                    }
                    each_child(m, |k| {
                        let ch = unsafe { k.as_ref() };
                        if ch.has(flag::MEMBER) {
                            bump(ch);
                        }
                    });
                }
                c.i = 0;
                c.phase = Phase::Scan;
                None
            }
            Phase::Scan => {
                if !scan(c, b) {
                    return Some(false);
                }
                // 4. Verify, in one go.
                let mut all = Budget { until: None, n: 0 };
                c.i = 0;
                scan(c, &mut all);
                c.i = 0;
                c.garbage.clear();
                c.phase = Phase::Collect;
                None
            }
            Phase::Collect => {
                while c.i < c.members.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let m = c.members[c.i];
                    c.i += 1;
                    let h = unsafe { m.as_ref() };
                    if color(h) == GRAY && !h.has(flag::DEAD) && !c.abort {
                        set_color(h, WHITE);
                        // No count reaches zero while the garbage drops.
                        h.rc.set(u32::MAX / 2);
                        h.set(flag::ARENA);
                        c.garbage.push(m);
                    }
                }
                if let Some(est) = c.census {
                    super::census::after_collect(c.roots, c.members.len(), &c.garbage, c.work.as_nanos() as u64, est);
                }
                c.i = 0;
                c.phase = Phase::Clear;
                None
            }
            Phase::Clear => {
                while c.i < c.members.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let i = c.i;
                    let m = c.members[i];
                    c.i += 1;
                    let h = unsafe { m.as_ref() };
                    if color(h) == WHITE {
                        continue;
                    }
                    let d = indeg(h).min(flag::INDEG_MAX);
                    h.flags.set(h.flags.get() & !(flag::MEMBER | flag::TOUCHED | flag::INDEG_MASK));
                    set_color(h, BLACK);
                    if h.has(flag::DEAD) {
                        // Released during the check; a list frees a listed one.
                        if h.flags.get() & (flag::BUFFERED | flag::HELD) == 0 {
                            free_box(m);
                        }
                    } else if h.owner.get() == 0 {
                        if c.abort {
                            // Nothing was decided: the candidates stay candidates.
                            if i < c.roots && !h.has(flag::BUFFERED) {
                                h.set(flag::BUFFERED);
                                st().roots.push(m);
                            }
                        } else {
                            // A survivor: what keeps it alive may be a
                            // reference to an owned box of its structure,
                            // whose loss buffers nothing. So every full
                            // check looks at it again.
                            h.flags.set(h.flags.get() | flag::SURVIVOR | d << flag::INDEG_SHIFT);
                            if !h.has(flag::HELD) {
                                h.set(flag::HELD);
                                c.old.push(m);
                            }
                        }
                    }
                }
                c.i = 0;
                c.phase = Phase::Drop;
                None
            }
            Phase::Drop => {
                while c.i < c.garbage.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let g = c.garbage[c.i];
                    c.i += 1;
                    // Live boxes it owns lose their owner before it's freed.
                    if unsafe { g.as_ref() }.has(flag::OWNS) {
                        super::heap::orphan_children(g);
                    }
                    unsafe { super::heap::drop_header_value(g) };
                }
                c.i = 0;
                c.phase = Phase::Free;
                None
            }
            Phase::Free => {
                while c.i < c.garbage.len() {
                    if b.out() {
                        return Some(false);
                    }
                    let g = c.garbage[c.i];
                    c.i += 1;
                    let h = unsafe { g.as_ref() };
                    h.flags.set(h.flags.get() & !(flag::MEMBER | flag::TOUCHED | flag::INDEG_MASK));
                    set_color(h, BLACK);
                    if h.flags.get() & (flag::BUFFERED | flag::HELD) != 0 {
                        // On a list, which frees it.
                        h.set(flag::DEAD);
                    } else {
                        free_box(g);
                    }
                }
                STATS.collected.set(STATS.collected.get() + c.garbage.len() as u64);
                if c.full {
                    let live = live_bytes();
                    let own = live.saturating_sub(super::heap::START_BYTES.load(std::sync::atomic::Ordering::Relaxed));
                    NEXT.store(live + (own / 8).max(GROWTH), std::sync::atomic::Ordering::Relaxed);
                }
                c.phase = Phase::Idle;
                None
            }
        }
    }
}

/// Propagate liveness (steps 3 and 4) from the members with outside
/// evidence, from `c.i` on (and what is left on the stack). True when done.
#[cfg(feature = "owners")]
fn scan(c: &mut Check, b: &mut Budget) -> bool {
    loop {
        while let Some(n) = c.stack.pop() {
            if b.out() {
                c.stack.push(n);
                return false;
            }
            if unsafe { n.as_ref() }.has(flag::DEAD) {
                continue;
            }
            super::heap::OPAQUE.store(false, std::sync::atomic::Ordering::Relaxed);
            let stack = &mut c.stack;
            each_child(n, |k| {
                let ch = unsafe { k.as_ref() };
                if ch.has(flag::MEMBER) && color(ch) == GRAY {
                    set_color(ch, BLACK);
                    stack.push(k);
                }
            });
            if super::heap::OPAQUE.load(std::sync::atomic::Ordering::Relaxed) {
                // A live member's children are out of sight: what they
                // reach can't be told from garbage.
                c.abort = true;
            }
        }
        if c.i >= c.members.len() {
            return true;
        }
        if b.out() {
            return false;
        }
        let m = c.members[c.i];
        c.i += 1;
        let h = unsafe { m.as_ref() };
        if seed(h) {
            set_color(h, BLACK);
            c.stack.push(m);
        }
    }
}

/// `RTS_CC_LOG`: a line per check.
#[cfg(feature = "owners")]
fn log_check(c: &Check) {
    eprintln!(
        "cc {} us in {} slices (longest {} us): {} roots, {} members, {} garbage{}{} | phases us {:?}",
        c.work.as_micros(),
        c.slices,
        c.longest.as_micros(),
        c.roots,
        c.members.len(),
        c.garbage.len(),
        if c.full { ", full" } else { "" },
        if c.abort { ", abandoned" } else { "" },
        c.phase_ns.map(|n| n / 1000)
    );
    eprintln!("   live {} KB, next {} KB, threshold {}, buffered {}, old {}", live_bytes() >> 10, NEXT.load(std::sync::atomic::Ordering::Relaxed) >> 10, c.threshold, st().roots.len(), c.old.len());
}

/// Run a slice of the ownership check (starting one if none is under way).
#[cfg(feature = "owners")]
pub fn collect_owned() {
    let c = check();
    let now = Instant::now();
    let s = st();
    if c.phase == Phase::Idle {
        if !grown() {
            // The buffer filled: compact it, and check only if enough
            // candidates are left.
            compact(c);
            if s.roots.len() < if s.fixed { s.threshold } else { c.threshold } {
                STATS.pause(now.elapsed(), super::memtrace::Kind::Cycles);
                return;
            }
        }
        begin(c);
    } else if s.roots.len() >= s.kept + compact_every(s) {
        compact(c);
    }
    let us = SLICE_US.load(std::sync::atomic::Ordering::Relaxed);
    // The census looks at the heap between checks: each in one go. So
    // does deferred counting, whose stack references aren't counted.
    let whole = us == 0 || super::census::on() || super::zct::deferred();
    let mut b = Budget { until: if whole { None } else { Some(now + std::time::Duration::from_micros(us)) }, n: 0 };
    let done = run(c, &mut b);
    let d = now.elapsed();
    c.work += d;
    c.slices += 1;
    c.longest = c.longest.max(d);
    if done {
        SLICE_AT.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        if std::env::var_os("RTS_CC_LOG").is_some() {
            log_check(c);
        }
        // An ordinary check that found little: wait for more candidates
        // next time (full checks bound what that leaves uncollected).
        if !c.full {
            c.threshold = if c.garbage.len() * 8 < c.members.len() { (c.threshold * 2).min(MAX_THRESHOLD) } else { (c.threshold / 2).max(MIN_THRESHOLD) };
        }
        c.members = Vec::new();
        c.garbage = Vec::new();
        c.excess = Default::default();
        c.stack = Vec::new();
    } else {
        // Paced by allocation: the program allocates in proportion to the
        // check's progress, so garbage can't pile up faster than it's found.
        SLICE_AT.store(total_bytes() + (b.n as u64 * BYTES_PER_STEP.load(std::sync::atomic::Ordering::Relaxed)).max(SLICE_GATE), std::sync::atomic::Ordering::Relaxed);
    }
    STATS.pause(d, super::memtrace::Kind::Cycles);
}

/// Candidates currently buffered.
pub fn buffered() -> usize {
    st().roots.len()
}
