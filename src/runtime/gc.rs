//! Tracing mark-sweep garbage collection (`--features gc`), the baseline the
//! reference-counting and scope-arena modes are compared against.
//!
//! In this build `Gc` pointers carry no counts: `clone` and `drop` do nothing.
//! Every box is on one intrusive list. When the heap has grown enough since
//! the last collection (live bytes twice what survived it, at least
//! `MIN_HEAP`), the VM collects at its next safepoint: mark everything
//! reachable from its roots, then sweep the list, freeing unmarked boxes.
//!
//! Safepoints are function entries in the interpreter, where every live value
//! is in a VM slot or a VM structure. Native code that calls back into the VM
//! may hold values in Rust locals, so collection is postponed while such a
//! call is active (`Vm::gc_block`).

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use super::heap::{drop_header_value, flag, free_box, trace_header, Header, Tracer, LIVE_BYTES, STATS};

/// Don't collect before the heap reaches this size.
const MIN_HEAP: usize = 8 << 20;

struct State {
    head: Option<NonNull<Header>>,
    /// Collect when live bytes exceed this.
    next: usize,
    /// Smallest heap to collect at (`RTS_GC_MIN_HEAP`; 0 collects at every safepoint).
    min: usize,
}

struct Global(UnsafeCell<State>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State { head: None, next: MIN_HEAP, min: MIN_HEAP }));

/// Collect once the heap reaches `n` bytes (0: at every safepoint, for testing).
pub fn set_min_heap(n: usize) {
    st().min = n;
    st().next = n;
}

#[allow(clippy::mut_from_ref)]
fn st() -> &'static mut State {
    unsafe { &mut *STATE.0.get() }
}

/// A new box: put it on the list.
#[inline]
pub fn register(h: NonNull<Header>) {
    let s = st();
    unsafe { h.as_ref() }.next.set(s.head);
    s.head = Some(h);
}

/// Whether the heap has grown enough to collect.
#[inline(always)]
pub fn wanted() -> bool {
    LIVE_BYTES.load(Relaxed) >= st().next
}

/// Collect. `roots` reports every root pointer to the tracer it is given.
pub fn collect(roots: &mut dyn FnMut(&mut Tracer)) {
    let start = Instant::now();
    // Mark.
    let mut stack: Vec<NonNull<Header>> = Vec::new();
    {
        let mut visit = |h: NonNull<Header>| {
            let hd = unsafe { h.as_ref() };
            if !hd.has(flag::MARK) {
                hd.set(flag::MARK);
                stack.push(h);
            }
        };
        roots(&mut Tracer { visit: &mut visit, shared: true });
    }
    while let Some(h) = stack.pop() {
        let mut visit = |c: NonNull<Header>| {
            let hd = unsafe { c.as_ref() };
            if !hd.has(flag::MARK) {
                hd.set(flag::MARK);
                stack.push(c);
            }
        };
        trace_header(h, &mut Tracer { visit: &mut visit, shared: true });
    }
    // Sweep: unlink and free unmarked boxes; unmark the rest. Values are
    // dropped first and boxes freed after, so a value's destructor never
    // sees a freed box.
    let s = st();
    let mut dead: Vec<NonNull<Header>> = Vec::new();
    let mut prev: Option<NonNull<Header>> = None;
    let mut cur = s.head;
    while let Some(h) = cur {
        let hd = unsafe { h.as_ref() };
        let next = hd.next.get();
        if hd.has(flag::MARK) {
            hd.clear(flag::MARK);
            prev = Some(h);
        } else {
            match prev {
                Some(p) => unsafe { p.as_ref() }.next.set(next),
                None => s.head = next,
            }
            dead.push(h);
        }
        cur = next;
    }
    for &h in &dead {
        unsafe { drop_header_value(h) };
    }
    for &h in &dead {
        free_box(h);
    }
    STATS.collected.set(STATS.collected.get() + dead.len() as u64);
    s.next = if s.min == 0 { 0 } else { (LIVE_BYTES.load(Relaxed) * 2).max(s.min) };
    STATS.pause(start.elapsed());
}
