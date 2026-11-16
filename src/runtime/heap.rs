//! The JavaScript heap.
//!
//! Every heap value (strings, objects, arrays, closures, …) is a `Gc<T>`: a
//! pointer to a `GcBox<T>`, which is a 16-byte header followed by the value.
//! How a box is freed depends on the memory mode:
//!
//! - **Reference counting** (the default build). The header holds a count;
//!   the box is freed the moment it drops to zero. Boxes that might be part of
//!   a reference cycle are remembered when their count drops to a non-zero
//!   value, and the cycle collector (`cycles.rs`) checks those candidates.
//! - **Scope arenas** (also the default build). A box the compiler proved
//!   never outlives a scope is allocated in that scope's arena (`arena.rs`)
//!   instead of with `malloc`. Its count starts at `ARENA_COUNT`, so
//!   reference-count traffic never frees it; the arena frees all of its boxes
//!   at once when the scope exits.
//! - **Tracing GC** (`--features gc`). Counts are not maintained at all:
//!   `clone` and `drop` are no-ops. Every box is on an intrusive list, and the
//!   collector (`gc.rs`) marks from the VM's roots and sweeps the rest.
//!
//! Allocation goes through the stable `std::alloc::Allocator` trait: `Global`
//! for ordinary boxes, an arena region for scope-allocated ones.

use std::alloc::{Allocator, GlobalAlloc, Global, Layout, System};
use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Visits the `Gc` pointers held by a value.
pub trait Trace {
    /// Values of this type never hold a `Gc` pointer (strings, BigInts, …),
    /// so they can never be part of a cycle.
    const ACYCLIC: bool = false;
    fn trace(&self, t: &mut Tracer);
}

/// Receives each child pointer of a traced value.
pub struct Tracer<'a> {
    pub visit: &'a mut dyn FnMut(NonNull<Header>),
    /// Also report pointers held by shared, non-`Gc` containers (shapes,
    /// classes, promise combinators). The tracing GC wants them (marking is
    /// idempotent); the cycle collector must not see them, because such a
    /// container holds one counted reference however many boxes share it.
    pub shared: bool,
}

impl Tracer<'_> {
    #[inline]
    pub fn edge<T: Trace + 'static>(&mut self, g: &Gc<T>) {
        (self.visit)(g.ptr.cast());
    }
}

/// Header flags.
pub mod flag {
    /// The value can never hold a `Gc` pointer, so it can't be in a cycle.
    pub const ACYCLIC: u32 = 1 << 0;
    /// In the cycle collector's candidate buffer.
    pub const BUFFERED: u32 = 1 << 1;
    /// Allocated in a scope arena: never freed by its count.
    pub const ARENA: u32 = 1 << 2;
    /// Value dropped while buffered; the box itself is freed by the collector.
    pub const DEAD: u32 = 1 << 3;
    /// Cycle collector colours (Bacon & Rajan): two bits.
    pub const COLOR_SHIFT: u32 = 4;
    pub const COLOR_MASK: u32 = 3 << COLOR_SHIFT;
    /// Tracing GC mark bit.
    pub const MARK: u32 = 1 << 6;
}

/// Count given to arena boxes. Far above any real count, so reference-count
/// traffic never takes it to zero; at scope exit it is back to this value
/// if the escape analysis was right (checked in debug builds).
pub const ARENA_COUNT: u32 = 1 << 30;

#[repr(C)]
pub struct Header {
    pub rc: Cell<u32>,
    pub flags: Cell<u32>,
    pub vt: &'static VTable,
    /// The next box in the tracing GC's list of all boxes.
    #[cfg(feature = "gc")]
    pub next: Cell<Option<NonNull<Header>>>,
}

impl Header {
    #[inline]
    pub fn has(&self, f: u32) -> bool {
        self.flags.get() & f != 0
    }
    #[inline]
    pub fn set(&self, f: u32) {
        self.flags.set(self.flags.get() | f)
    }
    #[inline]
    pub fn clear(&self, f: u32) {
        self.flags.set(self.flags.get() & !f)
    }
}

/// Per-type operations on a type-erased box.
pub struct VTable {
    pub name: fn() -> &'static str,
    pub layout: Layout,
    pub trace: unsafe fn(NonNull<Header>, &mut Tracer),
    pub drop_value: unsafe fn(NonNull<Header>),
}

#[repr(C)]
struct GcBox<T> {
    hdr: Header,
    val: T,
}

struct Vt<T>(PhantomData<T>);

impl<T: Trace + 'static> Vt<T> {
    const V: VTable = VTable {
        name: std::any::type_name::<T>,
        layout: Layout::new::<GcBox<T>>(),
        trace: trace_box::<T>,
        drop_value: drop_box::<T>,
    };
}

unsafe fn trace_box<T: Trace>(h: NonNull<Header>, t: &mut Tracer) {
    unsafe { (*h.cast::<GcBox<T>>().as_ptr()).val.trace(t) }
}

unsafe fn drop_box<T>(h: NonNull<Header>) {
    unsafe { std::ptr::drop_in_place(&mut (*h.cast::<GcBox<T>>().as_ptr()).val) }
}

/// A pointer to a heap value. See the module docs for how it is freed.
pub struct Gc<T: Trace + 'static> {
    ptr: NonNull<GcBox<T>>,
    _own: PhantomData<T>,
}

impl<T: Trace + 'static> Gc<T> {
    /// Allocate with the global allocator.
    #[inline]
    pub fn new(val: T) -> Gc<T> {
        Self::new_in(val, &Global, if T::ACYCLIC { flag::ACYCLIC } else { 0 })
    }

    /// Allocate a value that never holds `Gc` pointers (a string, a number
    /// array, …): the cycle collector never looks at it.
    #[inline]
    pub fn new_acyclic(val: T) -> Gc<T> {
        Self::new_in(val, &Global, flag::ACYCLIC)
    }

    /// Allocate with any allocator. Arena boxes pass `flag::ARENA`.
    #[inline]
    pub fn new_in<A: Allocator>(val: T, alloc: &A, flags: u32) -> Gc<T> {
        let layout = Layout::new::<GcBox<T>>();
        let mem = match alloc.allocate(layout) {
            Ok(m) => m.cast::<GcBox<T>>(),
            Err(_) => std::alloc::handle_alloc_error(layout),
        };
        let rc = if flags & flag::ARENA != 0 { ARENA_COUNT } else { 1 };
        unsafe {
            mem.as_ptr().write(GcBox {
                hdr: Header {
                    rc: Cell::new(rc),
                    flags: Cell::new(flags),
                    vt: &Vt::<T>::V,
                    #[cfg(feature = "gc")]
                    next: Cell::new(None),
                },
                val,
            });
        }
        let g = Gc { ptr: mem, _own: PhantomData };
        #[cfg(feature = "gc")]
        if flags & flag::ARENA == 0 {
            crate::runtime::gc::register(g.header_ptr());
        }
        STATS.boxes.set(STATS.boxes.get() + 1);
        g
    }

    #[inline]
    pub fn header(&self) -> &Header {
        unsafe { &(*self.ptr.as_ptr()).hdr }
    }

    #[inline]
    pub fn header_ptr(&self) -> NonNull<Header> {
        self.ptr.cast()
    }

    #[inline]
    pub fn ptr_eq(a: &Gc<T>, b: &Gc<T>) -> bool {
        a.ptr == b.ptr
    }

    #[inline]
    pub fn as_ptr(g: &Gc<T>) -> *const T {
        unsafe { &(*g.ptr.as_ptr()).val as *const T }
    }

    pub fn count(g: &Gc<T>) -> u32 {
        g.header().rc.get()
    }

    /// Rebuild a `Gc` from a header pointer, taking a new reference.
    ///
    /// # Safety
    /// `h` must point at a live `GcBox<T>`.
    pub unsafe fn from_header(h: NonNull<Header>) -> Gc<T> {
        let g: Gc<T> = Gc { ptr: h.cast(), _own: PhantomData };
        inc(g.header());
        g
    }
}

impl<T: Trace + 'static> Deref for Gc<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        unsafe { &(*self.ptr.as_ptr()).val }
    }
}

impl<T: Trace + 'static> Clone for Gc<T> {
    #[inline(always)]
    fn clone(&self) -> Gc<T> {
        inc(self.header());
        Gc { ptr: self.ptr, _own: PhantomData }
    }
}

impl<T: Trace + 'static> Drop for Gc<T> {
    #[inline(always)]
    fn drop(&mut self) {
        dec(self.ptr.cast());
    }
}

impl<T: Trace + fmt::Display + 'static> fmt::Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

/// Compares the values, as `Rc` does.
impl<T: Trace + PartialEq + 'static> PartialEq for Gc<T> {
    fn eq(&self, other: &Gc<T>) -> bool {
        **self == **other
    }
}

impl<T: Trace + fmt::Debug + 'static> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(not(feature = "gc"))]
#[inline(always)]
fn inc(h: &Header) {
    h.rc.set(h.rc.get() + 1);
}

#[cfg(feature = "gc")]
#[inline(always)]
fn inc(_: &Header) {}

#[cfg(not(feature = "gc"))]
#[inline(always)]
fn dec(p: NonNull<Header>) {
    let h = unsafe { p.as_ref() };
    let c = h.rc.get() - 1;
    h.rc.set(c);
    if c == 0 {
        release(p);
    } else if h.flags.get() & (flag::ACYCLIC | flag::BUFFERED | flag::ARENA) == 0 {
        crate::runtime::cycles::possible_root(p);
    }
}

#[cfg(feature = "gc")]
#[inline(always)]
fn dec(_: NonNull<Header>) {}

/// The count reached zero: drop the value and free the box (unless the
/// cycle collector still holds it, or it lives in an arena).
#[cold]
#[inline(never)]
pub fn release(p: NonNull<Header>) {
    let h = unsafe { p.as_ref() };
    let vt = h.vt;
    if h.has(flag::BUFFERED) {
        // The collector frees the box when it next looks at its buffer.
        h.set(flag::DEAD);
        unsafe { (vt.drop_value)(p) };
        return;
    }
    unsafe { (vt.drop_value)(p) };
    if !h.has(flag::ARENA) {
        free_box(p);
    }
}

/// Return a box's memory (its value must already be dropped).
pub fn free_box(p: NonNull<Header>) {
    let layout = unsafe { p.as_ref() }.vt.layout;
    STATS.boxes.set(STATS.boxes.get() - 1);
    unsafe { Global.deallocate(p.cast(), layout) };
}

/// Trace a type-erased box's children.
pub fn trace_header(p: NonNull<Header>, t: &mut Tracer) {
    unsafe { (p.as_ref().vt.trace)(p, t) }
}

/// Drop a type-erased box's value in place.
///
/// # Safety
/// The value must not be used afterwards.
pub unsafe fn drop_header_value(p: NonNull<Header>) {
    unsafe { (p.as_ref().vt.drop_value)(p) }
}

// ───────────── statistics ─────────────

/// Single-threaded counters (the VM runs on one thread).
pub struct Stats {
    /// Live boxes.
    pub boxes: Cell<i64>,
    /// Collector pauses: count, total and longest, in nanoseconds.
    pub pauses: Cell<u64>,
    pub pause_total_ns: Cell<u64>,
    pub pause_max_ns: Cell<u64>,
    /// Boxes freed by the cycle collector or the tracing GC.
    pub collected: Cell<u64>,
    /// Arena statistics: boxes allocated in arenas, bytes, and scope exits that reset one.
    pub arena_boxes: Cell<u64>,
    pub arena_bytes: Cell<u64>,
    pub arena_resets: Cell<u64>,
}

pub struct SyncStats(UnsafeCell<Stats>);
// SAFETY: the VM, and so every user of these counters, runs on one thread.
unsafe impl Sync for SyncStats {}

impl Deref for SyncStats {
    type Target = Stats;
    fn deref(&self) -> &Stats {
        unsafe { &*self.0.get() }
    }
}

pub static STATS: SyncStats = SyncStats(UnsafeCell::new(Stats {
    boxes: Cell::new(0),
    pauses: Cell::new(0),
    pause_total_ns: Cell::new(0),
    pause_max_ns: Cell::new(0),
    collected: Cell::new(0),
    arena_boxes: Cell::new(0),
    arena_bytes: Cell::new(0),
    arena_resets: Cell::new(0),
}));

impl Stats {
    pub fn pause(&self, d: std::time::Duration) {
        let ns = d.as_nanos() as u64;
        self.pauses.set(self.pauses.get() + 1);
        self.pause_total_ns.set(self.pause_total_ns.get() + ns);
        self.pause_max_ns.set(self.pause_max_ns.get().max(ns));
    }
}

/// The process allocator, counting live and peak bytes.
pub struct CountingAlloc;

pub static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
pub static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
pub static TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
/// Live bytes when the program started running (compiled code and all).
pub static START_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The program starts running: measure the peak from here.
pub fn start_run() {
    let live = LIVE_BYTES.load(Relaxed);
    START_BYTES.store(live, Relaxed);
    PEAK_BYTES.store(live, Relaxed);
}

#[inline(always)]
fn grew(n: usize) {
    let live = LIVE_BYTES.fetch_add(n, Relaxed) + n;
    if live > PEAK_BYTES.load(Relaxed) {
        PEAK_BYTES.store(live, Relaxed);
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        TOTAL_ALLOCS.fetch_add(1, Relaxed);
        TOTAL_BYTES.fetch_add(l.size() as u64, Relaxed);
        grew(l.size());
        unsafe { System.alloc(l) }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        TOTAL_ALLOCS.fetch_add(1, Relaxed);
        TOTAL_BYTES.fetch_add(l.size() as u64, Relaxed);
        grew(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_BYTES.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n > l.size() {
            TOTAL_BYTES.fetch_add((n - l.size()) as u64, Relaxed);
            grew(n - l.size());
        } else {
            LIVE_BYTES.fetch_sub(l.size() - n, Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}

/// `--mem-stats`: print heap statistics to stderr.
pub fn print_stats() {
    let s = &*STATS;
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    eprintln!("rts memory statistics");
    let base = START_BYTES.load(Relaxed) as u64;
    eprintln!("  peak heap        {:>10.2} MB (above the {:.2} MB of loaded code)", mb(PEAK_BYTES.load(Relaxed) as u64 - base), mb(base));
    eprintln!("  live heap at end {:>10.2} MB", mb(LIVE_BYTES.load(Relaxed) as u64));
    eprintln!("  allocations      {:>10}", TOTAL_ALLOCS.load(Relaxed));
    eprintln!("  allocated        {:>10.2} MB", mb(TOTAL_BYTES.load(Relaxed)));
    eprintln!("  live boxes       {:>10}", s.boxes.get());
    eprintln!("  arena boxes      {:>10} ({:.2} MB, {} scope resets)", s.arena_boxes.get(), mb(s.arena_bytes.get()), s.arena_resets.get());
    eprintln!("  collected        {:>10} boxes", s.collected.get());
    eprintln!(
        "  pauses           {:>10} (total {:.3} ms, max {:.3} ms)",
        s.pauses.get(),
        s.pause_total_ns.get() as f64 / 1e6,
        s.pause_max_ns.get() as f64 / 1e6
    );
}
