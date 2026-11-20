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
    enabled: bool,
}

struct Global(UnsafeCell<State>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State {
    roots: Vec::new(),
    threshold: 10_000,
    fixed: false,
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
    s.roots.len() >= s.threshold
        || (cfg!(feature = "owners")
            && !s.roots.is_empty()
            && super::heap::LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed) >= NEXT.load(std::sync::atomic::Ordering::Relaxed))
}

/// Owners build: check when live bytes reach this.
static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
/// Owners build: heap growth between checks while candidates wait: a
/// quarter of the program's heap, at least this.
const GROWTH: usize = 512 << 10;

/// The program starts (owners build): the growth trigger counts from here.
pub fn start_run() {
    NEXT.store(super::heap::LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed) + GROWTH, std::sync::atomic::Ordering::Relaxed);
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

/// Ownership's collection (`owners` build): candidates are boxes without
/// an owner whose count dropped to a non-zero value. Trial deletion runs
/// over what the candidates own (following owner links down, never a
/// borrow), not over everything they reach: a garbage cycle among owned
/// boxes is found when its root loses its last outside reference, at a
/// cost proportional to what the root owns.
///
/// 1. Members: the candidates and everything they own, transitively.
/// 2. Subtract the references between members from their counts.
/// 3. A member still counted is referenced from outside: it and the members
///    it reaches are live (counts restored). The rest is garbage.
/// 4. Live members owned by garbage lose their owner; free the garbage.
#[cfg(feature = "owners")]
pub fn collect_owned() {
    use super::heap::flag::MARK;
    let start = Instant::now();
    let s = st();
    let roots = std::mem::take(&mut s.roots);
    let mut live_roots = Vec::with_capacity(roots.len());
    for p in roots {
        let h = unsafe { p.as_ref() };
        if h.has(flag::DEAD) {
            free_box(p);
            continue;
        }
        h.clear(flag::BUFFERED);
        // Owned since it was buffered: its owner keeps it.
        if h.rc.get() > 0 && !h.has(flag::OWNED) {
            live_roots.push(p);
        }
    }
    let census = if super::census::on() { Some(super::census::before_collect(&live_roots)) } else { None };

    // 1. Members, gray.
    let mut members: Vec<NonNull<Header>> = Vec::with_capacity(live_roots.len());
    for &p in &live_roots {
        let h = unsafe { p.as_ref() };
        if !h.has(MARK) {
            h.set(MARK);
            set_color(h, GRAY);
            members.push(p);
        }
    }
    let mut i = 0;
    while i < members.len() {
        let m = members[i];
        i += 1;
        let a = m.as_ptr() as usize;
        // What it owns, and the other unowned roots it borrows: a garbage
        // structure made of several ownership trees linked by borrows dies
        // as a whole.
        each_child(m, |c| {
            let ch = unsafe { c.as_ref() };
            if ch.has(MARK) || ch.has(flag::ARENA) {
                return;
            }
            let o = ch.owner.get();
            if o == a || o == 0 {
                ch.set(MARK);
                set_color(ch, GRAY);
                members.push(c);
            }
        });
    }
    // 2. Trial deletion of the references between members.
    for &m in &members {
        each_child(m, |c| {
            let ch = unsafe { c.as_ref() };
            if ch.has(MARK) {
                ch.rc.set(ch.rc.get() - 1);
            }
        });
    }
    // 3. Members referenced from outside, and what they reach, are live.
    let mut stack: Vec<NonNull<Header>> = Vec::new();
    for &m in &members {
        let h = unsafe { m.as_ref() };
        if color(h) != GRAY || h.rc.get() == 0 {
            continue;
        }
        set_color(h, BLACK);
        stack.push(m);
        while let Some(n) = stack.pop() {
            each_child(n, |c| {
                let ch = unsafe { c.as_ref() };
                if ch.has(MARK) {
                    ch.rc.set(ch.rc.get() + 1);
                    if color(ch) == GRAY {
                        set_color(ch, BLACK);
                        stack.push(c);
                    }
                }
            });
        }
    }
    // 4. The rest is garbage.
    let mut garbage: Vec<NonNull<Header>> = Vec::new();
    for &m in &members {
        let h = unsafe { m.as_ref() };
        if color(h) == GRAY {
            set_color(h, WHITE);
            garbage.push(m);
        }
    }
    // Edges from garbage to live members were subtracted and not restored;
    // dropping the garbage subtracts them again.
    for &g in &garbage {
        each_child(g, |c| {
            let ch = unsafe { c.as_ref() };
            if ch.has(MARK) && color(ch) == BLACK {
                ch.rc.set(ch.rc.get() + 1);
            }
        });
    }
    for &m in &members {
        let h = unsafe { m.as_ref() };
        if color(h) == BLACK {
            h.clear(MARK);
            let o = h.owner.get();
            if o != 0 && color(unsafe { &*(o as *const Header) }) == WHITE && unsafe { &*(o as *const Header) }.has(MARK) {
                h.owner.set(0);
                h.clear(flag::OWNED);
            }
        }
    }
    if let Some(est) = census {
        super::census::after_collect(live_roots.len(), members.len(), &garbage, start.elapsed().as_nanos() as u64, est);
    }
    for &p in &garbage {
        let h = unsafe { p.as_ref() };
        h.rc.set(u32::MAX / 2);
        h.set(flag::ARENA);
    }
    for &p in &garbage {
        unsafe { super::heap::drop_header_value(p) };
    }
    for &p in &garbage {
        let h = unsafe { p.as_ref() };
        h.clear(MARK);
        set_color(h, BLACK);
        free_box(p);
    }
    STATS.collected.set(STATS.collected.get() + garbage.len() as u64);
    let live = super::heap::LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let own = live.saturating_sub(super::heap::START_BYTES.load(std::sync::atomic::Ordering::Relaxed));
    NEXT.store(live + (own / 4).max(GROWTH), std::sync::atomic::Ordering::Relaxed);
    STATS.pause(start.elapsed(), super::memtrace::Kind::Cycles);
}

/// Candidates currently buffered.
pub fn buffered() -> usize {
    st().roots.len()
}
