//! Scope arenas: bump allocation for objects that the escape analysis proved
//! never outlive a block (see `escape.rs`).
//!
//! Each active region (a block being executed that has scope-bound
//! allocations) owns an `Arena`. Boxes are bump-allocated from 32 KB chunks
//! through the `std::alloc::Allocator` trait, with the `ARENA` flag and count
//! `ARENA_COUNT` (so reference-count traffic never frees them). When the block
//! exits, the VM clears the block's slots and resets the arena: every box's
//! value is dropped (releasing what it points to) and the chunks go back to a
//! pool for the next region.
//!
//! The reset checks the analysis: a box whose count is still above
//! `ARENA_COUNT` once the other boxes are dropped is referenced from outside
//! the scope. That would be an escape-analysis bug; rather than free memory
//! that is still in use, its chunk is leaked (and counted in the statistics).

use std::alloc::{AllocError, Allocator, Global, Layout};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use super::heap::{drop_header_value, flag, Header, ARENA_COUNT, STATS};

const CHUNK: usize = 32 * 1024;

struct Chunk {
    ptr: NonNull<u8>,
    size: usize,
}

thread_local! {
    /// Free chunks, reused by later regions.
    static POOL: RefCell<Vec<Chunk>> = const { RefCell::new(Vec::new()) };
}

/// One region's allocator.
pub struct Arena {
    chunks: RefCell<Vec<Chunk>>,
    cur: Cell<usize>,
    end: Cell<usize>,
    /// Every box allocated here, in allocation order.
    boxes: RefCell<Vec<NonNull<Header>>>,
}

impl Default for Arena {
    fn default() -> Self {
        Arena {
            chunks: RefCell::new(Vec::new()),
            cur: Cell::new(0),
            end: Cell::new(0),
            boxes: RefCell::new(Vec::new()),
        }
    }
}

impl Arena {
    fn new_chunk(&self, need: usize) {
        let size = need.max(CHUNK);
        let chunk = if size == CHUNK { POOL.with(|p| p.borrow_mut().pop()) } else { None };
        let chunk = chunk.unwrap_or_else(|| {
            let l = Layout::from_size_align(size, 16).unwrap();
            let p = Global.allocate(l).expect("out of memory");
            Chunk { ptr: p.cast(), size }
        });
        self.cur.set(chunk.ptr.as_ptr() as usize);
        self.end.set(chunk.ptr.as_ptr() as usize + chunk.size);
        self.chunks.borrow_mut().push(chunk);
    }

    /// Record a box allocated here (called right after `Gc::new_in`).
    pub fn track(&self, h: NonNull<Header>) {
        self.boxes.borrow_mut().push(h);
    }

    /// The region exits: drop every box's value and recycle the chunks.
    #[inline]
    pub fn reset(&self) {
        if self.cur.get() == 0 {
            return; // nothing was allocated
        }
        self.reset_slow();
    }

    #[inline(never)]
    fn reset_slow(&self) {
        let mut boxes = self.boxes.borrow_mut();
        if !boxes.is_empty() {
            STATS.arena_resets.set(STATS.arena_resets.get() + 1);
            // Later boxes usually point to earlier ones, so drop newest first;
            // repeat while dropping frees up more.
            let mut pending: Vec<NonNull<Header>> = boxes.drain(..).rev().collect();
            loop {
                let before = pending.len();
                pending.retain(|&h| {
                    let hd = unsafe { h.as_ref() };
                    if hd.rc.get() == ARENA_COUNT {
                        hd.rc.set(ARENA_COUNT + 1); // pinned while its value drops
                        unsafe { drop_header_value(h) };
                        false
                    } else {
                        true
                    }
                });
                if pending.is_empty() || pending.len() == before {
                    break;
                }
            }
            if !pending.is_empty() {
                // Referenced from outside the scope: keep the memory alive.
                // The box behaves as an ordinary counted box from now on,
                // except that its memory is never returned.
                STATS.arena_escapes.set(STATS.arena_escapes.get() + pending.len() as u64);
                for &h in &pending {
                    let hd = unsafe { h.as_ref() };
                    hd.rc.set(hd.rc.get() - ARENA_COUNT);
                    debug_assert!(hd.has(flag::ARENA));
                }
                if std::env::var_os("RTS_ARENA_CHECK").is_some() {
                    eprintln!("rts: {} scope-arena object(s) outlived their scope", pending.len());
                }
                self.chunks.borrow_mut().clear(); // leaked
                self.cur.set(0);
                self.end.set(0);
                return;
            }
        }
        let mut chunks = self.chunks.borrow_mut();
        POOL.with(|p| {
            let mut p = p.borrow_mut();
            for c in chunks.drain(..) {
                if c.size == CHUNK && p.len() < 64 {
                    p.push(c);
                } else {
                    unsafe { Global.deallocate(c.ptr, Layout::from_size_align(c.size, 16).unwrap()) };
                }
            }
        });
        self.cur.set(0);
        self.end.set(0);
    }
}

unsafe impl Allocator for Arena {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let align = layout.align().max(16);
        let mut start = (self.cur.get() + align - 1) & !(align - 1);
        if self.cur.get() == 0 || start + layout.size() > self.end.get() {
            self.new_chunk(layout.size() + align);
            start = (self.cur.get() + align - 1) & !(align - 1);
        }
        self.cur.set(start + layout.size());
        STATS.arena_boxes.set(STATS.arena_boxes.get() + 1);
        STATS.arena_bytes.set(STATS.arena_bytes.get() + layout.size() as u64);
        let p = NonNull::new(start as *mut u8).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(p, layout.size()))
    }

    unsafe fn deallocate(&self, _: NonNull<u8>, _: Layout) {
        // Freed all at once by `reset`.
    }
}
