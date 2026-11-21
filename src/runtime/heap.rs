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
    /// Bytes allocated after the value in the same box (an object's inline
    /// fields; see `Gc::new_tail_in`).
    #[inline]
    fn tail_bytes(&self) -> usize {
        0
    }
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

/// Set when a trace skipped a mutably borrowed cell (its children unseen).
pub static OPAQUE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A trace skipped a mutably borrowed cell.
#[cold]
pub fn opaque() {
    OPAQUE.store(true, Relaxed);
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
    /// On the zero-count list (deferred reference counting).
    pub const ZCT: u32 = 1 << 7;
    /// Has an owner (`owners` build; see `own`).
    pub const OWNED: u32 = 1 << 8;
    /// Owns, or once owned, another box.
    pub const OWNS: u32 = 1 << 9;
    /// In the ownership check under way (`owners` build; see
    /// `cycles::collect_owned`). Above every `QUIET` bit: see `noisy`.
    pub const MEMBER: u32 = 1 << 10;
    /// A member whose count dropped during the check: live.
    pub const TOUCHED: u32 = 1 << 11;
    /// An unowned box a check found live. Its references from members at
    /// that check stay in the `INDEG` bits: while its count is higher, an
    /// ordinary check leaves it to the full check.
    pub const SURVIVOR: u32 = 1 << 12;
    /// On the collector's list of survivors left to the full check (which
    /// frees the box if its count reached zero meanwhile).
    pub const HELD: u32 = 1 << 13;
    /// During a check, a member's count of references from other members:
    /// three bits, the excess over 7 in the check's side table. (Bits 17
    /// and up hold an object's inline capacity.)
    pub const INDEG_SHIFT: u32 = 14;
    pub const INDEG_MAX: u32 = 7;
    pub const INDEG_MASK: u32 = INDEG_MAX << INDEG_SHIFT;
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
    /// The box holding this box's owning reference (0: none), `owners` build.
    #[cfg(feature = "owners")]
    pub owner: Cell<usize>,
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
    /// The box's layout (read before or after the value is dropped).
    pub layout: unsafe fn(NonNull<Header>) -> Layout,
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
        layout: layout_box::<T>,
        trace: trace_box::<T>,
        drop_value: drop_box::<T>,
    };
}

unsafe fn layout_box<T: Trace>(h: NonNull<Header>) -> Layout {
    let tail = unsafe { (*h.cast::<GcBox<T>>().as_ptr()).val.tail_bytes() };
    box_layout::<T>(tail)
}

fn box_layout<T>(tail: usize) -> Layout {
    let l = Layout::new::<GcBox<T>>();
    if tail == 0 { l } else { Layout::from_size_align(l.size() + tail, l.align()).unwrap() }
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
        unsafe { Self::new_tail_in(val, 0, |_| {}, alloc, flags) }
    }

    /// Allocate `val` followed by `tail` bytes, which `init` initialises
    /// (given their address) before the box is used.
    ///
    /// # Safety
    /// `val.tail_bytes()` must equal `tail`, and `init` must leave the tail in
    /// the state `T`'s `Drop` and `Trace` expect.
    #[inline]
    pub unsafe fn new_tail_in<A: Allocator>(val: T, tail: usize, init: impl FnOnce(*mut u8), alloc: &A, flags: u32) -> Gc<T> {
        let layout = box_layout::<T>(tail);
        let mem = match alloc.allocate(layout) {
            Ok(m) => m.cast::<GcBox<T>>(),
            Err(_) => std::alloc::handle_alloc_error(layout),
        };
        // One reference: the handle returned. Arena boxes count from ARENA_COUNT.
        let rc = if flags & flag::ARENA != 0 { ARENA_COUNT + 1 } else { 1 };
        unsafe {
            mem.as_ptr().write(GcBox {
                hdr: Header {
                    rc: Cell::new(rc),
                    flags: Cell::new(flags),
                    vt: &Vt::<T>::V,
                    #[cfg(feature = "owners")]
                    owner: Cell::new(0),
                    #[cfg(feature = "gc")]
                    next: Cell::new(None),
                },
                val,
            });
            if tail != 0 {
                init((mem.as_ptr() as *mut u8).add(size_of::<GcBox<T>>()));
            }
        }
        let g = Gc { ptr: mem, _own: PhantomData };
        #[cfg(feature = "gc")]
        if flags & flag::ARENA == 0 {
            crate::runtime::gc::register(g.header_ptr());
        }
        if flags & flag::ARENA == 0 {
            STATS.boxes.set(STATS.boxes.get() + 1);
            #[cfg(not(feature = "gc"))]
            if crate::runtime::census::on() {
                crate::runtime::census::born(g.header_ptr(), layout.size());
            }
        }
        STATS.allocated.set(STATS.allocated.get() + 1);
        g
    }

    /// Address of the bytes after the value (see `new_tail_in`).
    #[inline(always)]
    pub fn tail_ptr(&self) -> *mut u8 {
        unsafe { (self.ptr.as_ptr() as *mut u8).add(size_of::<GcBox<T>>()) }
    }

    /// Offset of the value from the start of the box.
    pub const VALUE_OFFSET: usize = std::mem::offset_of!(GcBox<T>, val);

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

    /// Give up the handle without releasing its reference (compiled code
    /// holds it as a raw pointer; see `from_header_owned`).
    pub fn into_header(g: Gc<T>) -> NonNull<Header> {
        let p = g.header_ptr();
        std::mem::forget(g);
        p
    }

    /// Rebuild a `Gc` from a header pointer that already owns a reference.
    ///
    /// # Safety
    /// `h` must point at a live `GcBox<T>` and carry a counted reference.
    pub unsafe fn from_header_owned(h: NonNull<Header>) -> Gc<T> {
        Gc { ptr: h.cast(), _own: PhantomData }
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

/// Take a reference to a box, given its header (used by `Value::clone`).
#[inline(always)]
pub fn inc_header(h: NonNull<Header>) {
    inc(unsafe { h.as_ref() });
}

/// Release a reference to a box, given its header (used by `Value`'s drop).
#[inline(always)]
pub fn dec_header(h: NonNull<Header>) {
    dec(h);
}

#[cfg(not(feature = "gc"))]
#[inline(always)]
fn inc(h: &Header) {
    #[cfg(feature = "profile")]
    STATS.rc_ops.set(STATS.rc_ops.get() + 1);
    h.rc.set(h.rc.get() + 1);
}

#[cfg(feature = "gc")]
#[inline(always)]
fn inc(_: &Header) {}

/// Boxes whose count dropping to a non-zero value can't make a garbage
/// cycle (or that are already candidates). With owners, an owned box can't:
/// a borrow went away, its owner still holds it.
pub const QUIET: u32 = flag::ACYCLIC | flag::BUFFERED | flag::ARENA | if cfg!(feature = "owners") { flag::OWNED } else { 0 };

/// Owners build: a decrement to a non-zero count takes the slow path when
/// the box has no `QUIET` bit, or is a member of the check under way (the
/// check's barrier: see `cycles::collect_owned`). One test: every `QUIET`
/// bit is below `MEMBER`, so `(flags & NOISY_MASK) - 1` wraps or lands at
/// `MEMBER - 1` or above exactly then.
pub const NOISY_MASK: u32 = QUIET | if cfg!(feature = "owners") { flag::MEMBER } else { 0 };
pub const NOISY_FROM: u32 = if cfg!(feature = "owners") { flag::MEMBER - 1 } else { u32::MAX };
const _: () = assert!(QUIET < flag::MEMBER);

#[inline(always)]
pub fn noisy(flags: u32) -> bool {
    (flags & NOISY_MASK).wrapping_sub(1) >= NOISY_FROM
}

#[cfg(not(feature = "gc"))]
#[inline(always)]
fn dec(p: NonNull<Header>) {
    #[cfg(feature = "profile")]
    STATS.rc_ops.set(STATS.rc_ops.get() + 1);
    let h = unsafe { p.as_ref() };
    let c = h.rc.get() - 1;
    h.rc.set(c);
    if c == 0 {
        if crate::runtime::zct::deferred() {
            crate::runtime::zct::push(p);
        } else {
            release(p);
        }
    } else if noisy(h.flags.get()) {
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
    // Tracing memory: time each outermost release (with everything it
    // frees in turn), as a pause if long.
    if crate::runtime::memtrace::on() {
        return crate::runtime::memtrace::timed_release(p, release_now);
    }
    release_now(p);
}

fn release_now(p: NonNull<Header>) {
    let h = unsafe { p.as_ref() };
    let vt = h.vt;
    // The boxes it owns lose their owner as its value drops.
    #[cfg(feature = "owners")]
    if h.has(flag::OWNS) {
        orphan_children(p);
    }
    if h.flags.get() & (flag::BUFFERED | if cfg!(feature = "owners") { flag::MEMBER | flag::HELD } else { 0 }) != 0 {
        // The collector frees the box when it next looks at its buffer
        // (or, for a member of the check under way, when the check ends).
        h.set(flag::DEAD);
        #[cfg(not(feature = "gc"))]
        if crate::runtime::census::on() {
            crate::runtime::census::freed(p);
        }
        unsafe { (vt.drop_value)(p) };
        return;
    }
    unsafe { (vt.drop_value)(p) };
    if !h.has(flag::ARENA) {
        free_box(p);
    }
}

// ───────────── ownership (`owners` build) ─────────────
//
// A box's owner is the box holding its first heap reference: an object's
// field, an array's element, a captured variable's cell. Other references
// are borrows. Owner links form a forest, and only a box without an owner
// can become a cycle candidate (see `cycles::collect_owned`). So every path
// that stores into one of those holders calls `own`, and every path that
// takes a reference out of one calls `disown` before releasing it: an
// owner link must never outlive the reference it stands for.

/// How far up the holder's owners `own` looks for the box itself.
#[cfg(feature = "owners")]
const OWN_DEPTH: usize = 64;

/// `child` was stored into `holder`: the holder owns it if nothing does
/// yet. If the holder is itself (transitively) owned by the child, the
/// store wins: structures are often built bottom-up (a child pointing to
/// its parent before the parent holds the child), so the older link in
/// that cycle, the one the child owns on the way to the holder, becomes a
/// borrow.
#[cfg(feature = "owners")]
#[inline]
pub fn own(holder: NonNull<Header>, child: NonNull<Header>) {
    let c = unsafe { child.as_ref() };
    if c.flags.get() & (flag::OWNED | flag::ACYCLIC | flag::ARENA) != 0 || holder == child {
        return;
    }
    own_slow(holder, child, true)
}

/// `child` was read from `holder`, so the holder holds it: if nothing owns
/// it (its owner went away while others still held it, as when an array is
/// copied to a bigger one), the holder does. Never demotes a link.
#[cfg(feature = "owners")]
#[inline]
pub fn adopt(holder: NonNull<Header>, child: NonNull<Header>) {
    let c = unsafe { child.as_ref() };
    if c.flags.get() & (flag::OWNED | flag::ACYCLIC | flag::ARENA) != 0 || holder == child {
        return;
    }
    own_slow(holder, child, false)
}

#[cfg(feature = "owners")]
#[inline(never)]
fn own_slow(holder: NonNull<Header>, child: NonNull<Header>, demote: bool) {
    let (h, c) = unsafe { (holder.as_ref(), child.as_ref()) };
    if h.has(flag::ARENA) {
        return;
    }
    if c.has(flag::OWNS) {
        // Is the holder (transitively) owned by the child?
        let mut q = holder.as_ptr() as usize;
        let mut n = 0;
        loop {
            let up = unsafe { (*(q as *const Header)).owner.get() };
            if up == 0 {
                break;
            }
            if up == child.as_ptr() as usize {
                if !demote {
                    return;
                }
                // `q` is owned by the child on the way to the holder.
                let qh = unsafe { &*(q as *const Header) };
                qh.owner.set(0);
                qh.clear(flag::OWNED);
                break;
            }
            n += 1;
            if n == OWN_DEPTH {
                return;
            }
            q = up;
        }
    }
    c.owner.set(holder.as_ptr() as usize);
    c.set(flag::OWNED);
    h.set(flag::OWNS);
}

/// The reference from `holder` to `child` is going away: if it was the
/// owning one, the child has no owner now.
#[cfg(feature = "owners")]
#[inline]
pub fn disown(holder: NonNull<Header>, child: NonNull<Header>) {
    let c = unsafe { child.as_ref() };
    if c.has(flag::OWNED) && c.owner.get() == holder.as_ptr() as usize {
        c.owner.set(0);
        c.clear(flag::OWNED);
    }
}

/// `own` for a value (a no-op unless it's a box).
#[cfg(feature = "owners")]
#[inline]
pub fn own_value(holder: NonNull<Header>, v: &crate::runtime::Value) {
    if let Some(c) = v.gc_header() {
        own(holder, c);
    }
}

/// `adopt` for a value.
#[cfg(feature = "owners")]
#[inline]
pub fn adopt_value(holder: NonNull<Header>, v: &crate::runtime::Value) {
    if let Some(c) = v.gc_header() {
        adopt(holder, c);
    }
}

/// `disown` for a value.
#[cfg(feature = "owners")]
#[inline]
pub fn disown_value(holder: NonNull<Header>, v: &crate::runtime::Value) {
    if let Some(c) = v.gc_header() {
        disown(holder, c);
    }
}

/// The box holding a value (`v` must be the value of a heap box).
#[cfg(feature = "owners")]
#[inline]
pub fn header_of<T: Trace + 'static>(v: &T) -> NonNull<Header> {
    unsafe { NonNull::new_unchecked((v as *const T as *mut u8).sub(Gc::<T>::VALUE_OFFSET) as *mut Header) }
}

/// A box is being freed: what it owns has no owner now.
#[cfg(feature = "owners")]
pub fn orphan_children(p: NonNull<Header>) {
    let a = p.as_ptr() as usize;
    let mut visit = |c: NonNull<Header>| {
        let ch = unsafe { c.as_ref() };
        if ch.owner.get() == a {
            ch.owner.set(0);
            ch.clear(flag::OWNED);
        }
    };
    trace_header(p, &mut Tracer { visit: &mut visit, shared: false });
}

/// Return a box's memory (its value must already be dropped).
pub fn free_box(p: NonNull<Header>) {
    #[cfg(not(feature = "gc"))]
    if crate::runtime::census::on() {
        crate::runtime::census::freed(p);
    }
    let layout = unsafe { (p.as_ref().vt.layout)(p) };
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
    /// Boxes allocated, in total (heap and arenas).
    pub allocated: Cell<u64>,
    /// Reference-count increments and decrements (counted in profile builds only).
    pub rc_ops: Cell<u64>,
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
    /// Arena boxes still referenced when their scope ended (should be 0).
    pub arena_escapes: Cell<u64>,
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
    allocated: Cell::new(0),
    rc_ops: Cell::new(0),
    pauses: Cell::new(0),
    pause_total_ns: Cell::new(0),
    pause_max_ns: Cell::new(0),
    collected: Cell::new(0),
    arena_boxes: Cell::new(0),
    arena_bytes: Cell::new(0),
    arena_resets: Cell::new(0),
    arena_escapes: Cell::new(0),
}));

impl Stats {
    pub fn pause(&self, d: std::time::Duration, kind: crate::runtime::memtrace::Kind) {
        let ns = d.as_nanos() as u64;
        self.pauses.set(self.pauses.get() + 1);
        self.pause_total_ns.set(self.pause_total_ns.get() + ns);
        self.pause_max_ns.set(self.pause_max_ns.get().max(ns));
        crate::runtime::memtrace::pause(d, kind);
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
    #[cfg(not(feature = "gc"))]
    crate::runtime::cycles::start_run();
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
    eprintln!("  objects          {:>10} allocated, {} live at end", s.allocated.get(), s.boxes.get());
    eprintln!("  arena boxes      {:>10} ({:.2} MB, {} scope resets, {} outlived their scope)", s.arena_boxes.get(), mb(s.arena_bytes.get()), s.arena_resets.get(), s.arena_escapes.get());
    eprintln!("  collected        {:>10} boxes", s.collected.get());
    eprintln!(
        "  pauses           {:>10} (total {:.3} ms, max {:.3} ms)",
        s.pauses.get(),
        s.pause_total_ns.get() as f64 / 1e6,
        s.pause_max_ns.get() as f64 / 1e6
    );
}
