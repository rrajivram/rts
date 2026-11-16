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
    let s = st();
    s.roots.len() >= s.threshold
}

/// The children of a box, skipping acyclic ones.
fn children(p: NonNull<Header>, out: &mut Vec<NonNull<Header>>) {
    out.clear();
    let mut visit = |c: NonNull<Header>| {
        if !unsafe { c.as_ref() }.has(flag::ACYCLIC) {
            out.push(c);
        }
    };
    trace_header(p, &mut Tracer { visit: &mut visit, shared: false });
}

/// Run a collection. Call only at a safepoint: no `RefCell` of a heap value
/// may be mutably borrowed (those that are, are treated as opaque, which is
/// safe: their children just look externally referenced).
pub fn collect() {
    let start = Instant::now();
    let s = st();
    let roots = std::mem::take(&mut s.roots);
    let mut kids = Vec::new();
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
        STATS.pause(start.elapsed());
        return;
    }
    for &p in &live_roots {
        let h = unsafe { p.as_ref() };
        if color(h) == GRAY {
            continue;
        }
        set_color(h, GRAY);
        stack.push(p);
        while let Some(n) = stack.pop() {
            children(n, &mut kids);
            for &c in &kids {
                let ch = unsafe { c.as_ref() };
                ch.rc.set(ch.rc.get() - 1);
                if color(ch) != GRAY {
                    set_color(ch, GRAY);
                    stack.push(c);
                }
            }
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
            if h.rc.get() > 0 {
                set_color(h, BLACK);
                black.push(n);
                while let Some(m) = black.pop() {
                    children(m, &mut kids);
                    for &c in &kids {
                        let ch = unsafe { c.as_ref() };
                        ch.rc.set(ch.rc.get() + 1);
                        if color(ch) != BLACK {
                            set_color(ch, BLACK);
                            black.push(c);
                        }
                    }
                }
            } else {
                set_color(h, WHITE);
                children(n, &mut kids);
                stack.extend_from_slice(&kids);
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
                children(n, &mut kids);
                stack.extend_from_slice(&kids);
            }
        }
    }

    // Trial deletion left every edge from a garbage box to a live box
    // subtracted. The garbage values are about to be dropped normally, which
    // subtracts those edges again, so restore them first.
    for &p in &garbage {
        children(p, &mut kids);
        for &c in &kids {
            let ch = unsafe { c.as_ref() };
            if !ch.has(flag::MARK) {
                ch.rc.set(ch.rc.get() + 1);
            }
        }
    }
    // Free them: first pin every white box (so the references between them,
    // released while their values drop, can't free a box twice), then drop
    // all values, then free all boxes.
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
    STATS.pause(start.elapsed());
}

/// Candidates currently buffered.
pub fn buffered() -> usize {
    st().roots.len()
}
