//! Tracing mark-sweep garbage collection (`--features gc`), the baseline the
//! reference-counting and scope-arena modes are compared against.
//!
//! In this build `Gc` pointers carry no counts: `clone` and `drop` do nothing.
//! Every box is on one intrusive list. When the heap has grown enough since
//! the last collection (live bytes twice what survived it, at least
//! `MIN_HEAP`), the VM collects at its next safepoint: mark everything
//! reachable from its roots, then sweep the list, freeing unmarked boxes.
//!
//! Safepoints are function entries in the interpreter and, in compiled code,
//! function entries and loop heads. Compiled code keeps no stack maps: its
//! values are found by scanning the machine stack (and callee-saved
//! registers) conservatively for box addresses, which can only keep extra
//! boxes alive, never free a live one (boxes don't move). Built-ins that
//! call back into the VM may hold values in Rust heap temporaries the scan
//! can't see, so collection is postponed while such a call is active
//! (`Vm::gc_block`).
//!
//! `RTS_GC_GROW` sets the growth factor (default 2: collect when live bytes
//! reach twice what survived the last collection), for time-space curves.

use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use super::heap::{drop_header_value, flag, free_box, trace_header, Header, Tracer, LIVE_BYTES, STATS};

/// Don't collect before the heap reaches this size.
const MIN_HEAP: usize = 8 << 20;
/// A stack word this far past a box's address (header and value start,
/// whatever the value's alignment) keeps the box.
const SLACK: usize = 48;

struct State {
    head: Option<NonNull<Header>>,
    /// Smallest heap to collect at (`RTS_GC_MIN_HEAP`; 0 collects at every safepoint).
    min: usize,
    /// Heap growth factor between collections (`RTS_GC_GROW`).
    grow: f64,
}

/// Collect when live bytes reach this (compiled code reads it at safepoints).
pub static NEXT: AtomicUsize = AtomicUsize::new(MIN_HEAP);
/// The highest stack address that can hold roots (set as the program starts).
static STACK_TOP: AtomicUsize = AtomicUsize::new(0);

struct Global(UnsafeCell<State>);
// SAFETY: the VM, and so every user of this state, runs on one thread.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State { head: None, min: MIN_HEAP, grow: 2.0 }));

/// Collect once the heap reaches `n` bytes (0: at every safepoint, for testing).
pub fn set_min_heap(n: usize) {
    st().min = n;
    NEXT.store(n, Relaxed);
}

/// Collect when the heap reaches `g` times what survived the last collection.
pub fn set_grow(g: f64) {
    st().grow = g.max(1.0);
}

/// Record where the stack starts (an address in a frame that outlives
/// every frame holding program values).
pub fn set_stack_top(addr: usize) {
    if STACK_TOP.load(Relaxed) < addr {
        STACK_TOP.store(addr, Relaxed);
    }
}

/// Every word on the stack from here up to `STACK_TOP`, callee-saved
/// registers included (stored into this frame first).
#[inline(never)]
fn stack_words() -> Vec<usize> {
    let mut regs = [0usize; 12];
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{0}]", "stp x21, x22, [{0}, #16]", "stp x23, x24, [{0}, #32]",
            "stp x25, x26, [{0}, #48]", "stp x27, x28, [{0}, #64]", "stp x29, x30, [{0}, #80]",
            in(reg) regs.as_mut_ptr(), options(nostack, preserves_flags),
        );
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "mov [{0}], rbx", "mov [{0} + 8], rbp", "mov [{0} + 16], r12",
            "mov [{0} + 24], r13", "mov [{0} + 32], r14", "mov [{0} + 40], r15",
            in(reg) regs.as_mut_ptr(), options(nostack, preserves_flags),
        );
    }
    let lo = regs.as_ptr() as usize;
    let hi = STACK_TOP.load(Relaxed);
    let mut out = Vec::new();
    let mut p = lo;
    while p + 8 <= hi {
        // SAFETY: between this frame and the recorded top: mapped stack.
        out.push(unsafe { std::ptr::read_volatile(p as *const usize) });
        p += 8;
    }
    std::hint::black_box(&regs);
    out
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
    LIVE_BYTES.load(Relaxed) >= NEXT.load(Relaxed)
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
        // Conservative roots: stack words equal to, or a little past, a
        // box's address (its header, or its value just after).
        let mut words = stack_words();
        words.sort_unstable();
        words.dedup();
        if let (Some(&lo), Some(&hi)) = (words.first(), words.last()) {
            let mut cur = st().head;
            while let Some(h) = cur {
                let a = h.as_ptr() as usize;
                if a >= lo.saturating_sub(SLACK) && a <= hi {
                    let i = words.partition_point(|&w| w < a);
                    if words.get(i).is_some_and(|&w| w < a + SLACK) {
                        visit(h);
                    }
                }
                cur = unsafe { h.as_ref() }.next.get();
            }
        }
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
    NEXT.store(if s.min == 0 { 0 } else { ((LIVE_BYTES.load(Relaxed) as f64 * s.grow) as usize).max(s.min) }, Relaxed);
    STATS.pause(start.elapsed());
}
