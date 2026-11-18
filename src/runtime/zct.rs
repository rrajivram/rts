//! Deferred reference counting (`--deferred-rc`, after Deutsch & Bobrow).
//!
//! Compiled code doesn't count the references its variables, arguments and
//! held temporaries make: those live on the machine stack. So a count
//! reaching zero only means no *heap* reference (field, element, cell,
//! interpreter slot, Rust value) is left. The box goes on the zero-count
//! list instead of being freed. At a safepoint, `reconcile` scans the stack
//! (conservatively, see `stack`) and the interpreter's slots, and frees every
//! listed box nothing there refers to; the children that reach zero as it
//! drops join the list and are settled in the same pass. Boxes still
//! referenced stay listed until a later pass.
//!
//! Every path that frees a box on its count reaching zero goes through
//! `heap::dec`, so in this mode nothing is freed while the stack may still
//! refer to it. The cycle collector settles the list first and treats boxes
//! the stack refers to as externally referenced.

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use super::heap::{flag, release, Header, LIVE_BYTES, STATS};

struct State {
    list: Vec<NonNull<Header>>,
    on: bool,
    /// The list length to settle at (`RTS_ZCT`).
    base: usize,
    /// … raised while many listed boxes stay referenced.
    limit: usize,
}

struct Global(UnsafeCell<State>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State { list: Vec::new(), on: false, base: 4096, limit: 4096 }));

/// Settle when live bytes reach this (compiled code compares the two): a
/// margin above what was live after the last settle, so garbage held only
/// by listed boxes stays bounded even when few boxes are listed (one
/// dropped root can hold a whole tree). A long list sets it to zero.
pub static NEXT: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Settle at least this often in heap growth.
const MARGIN: usize = 4 << 20;

#[allow(clippy::mut_from_ref)]
fn st() -> &'static mut State {
    unsafe { &mut *STATE.0.get() }
}

/// Turn deferred reference counting on (before any code runs).
/// `limit`: settle when this many boxes are listed (`RTS_ZCT`).
pub fn enable(limit: Option<usize>) {
    let s = st();
    s.on = true;
    if let Some(n) = limit {
        s.base = n.max(1);
    }
    s.limit = s.base;
    next_at(LIVE_BYTES.load(Relaxed));
}

fn next_at(live: usize) {
    NEXT.store(live + (live / 4).max(MARGIN), Relaxed);
}

#[inline(always)]
pub fn deferred() -> bool {
    st().on
}

/// A count reached zero: list the box (once).
#[cold]
#[inline(never)]
pub fn push(p: NonNull<Header>) {
    let h = unsafe { p.as_ref() };
    if h.has(flag::ZCT) {
        return;
    }
    h.set(flag::ZCT);
    let s = st();
    s.list.push(p);
    if s.list.len() >= s.limit {
        NEXT.store(0, Relaxed);
    }
}

/// Whether enough boxes are listed to settle.
#[inline(always)]
pub fn wanted() -> bool {
    LIVE_BYTES.load(Relaxed) >= NEXT.load(Relaxed)
}

/// Free every listed box that `roots` (sorted addresses: the stack's words
/// and the interpreter's slots) doesn't refer to. Returns the roots used:
/// every stack word if `all` (for the cycle collector), else only those
/// that could point into a listed box.
pub fn reconcile(extra: &[usize], all: bool) -> Vec<usize> {
    let start = Instant::now();
    let list = &st().list;
    let (lo, hi) = if all {
        (0, usize::MAX)
    } else {
        let lo = list.iter().map(|p| p.as_ptr() as usize).min().unwrap_or(0);
        (lo, list.iter().map(|p| p.as_ptr() as usize).max().map_or(0, |a| a + super::stack::SLACK))
    };
    let mut roots = super::stack::words_in(lo, hi);
    if !extra.is_empty() {
        roots.extend_from_slice(extra);
        roots.sort_unstable();
        roots.dedup();
    }
    let mut keep = Vec::new();
    // `release` lists more boxes as it runs: fetch the state afresh each time.
    while let Some(p) = st().list.pop() {
        let h = unsafe { p.as_ref() };
        if h.rc.get() > 0 {
            h.clear(flag::ZCT);
            continue;
        }
        if super::stack::refers(&roots, p.as_ptr() as usize) {
            keep.push(p);
            continue;
        }
        h.clear(flag::ZCT);
        // Its children's counts may reach zero: they join the list.
        release(p);
    }
    let s = st();
    s.list = keep;
    // Settle less often while many listed boxes stay referenced.
    s.limit = s.base.max(2 * s.list.len());
    next_at(LIVE_BYTES.load(Relaxed));
    if s.list.len() >= s.limit {
        NEXT.store(0, Relaxed);
    }
    STATS.pause(start.elapsed(), super::memtrace::Kind::Settle);
    roots
}
