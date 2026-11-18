//! Conservative scanning of the machine stack, for collectors that must find
//! references compiled code holds without stack maps: the tracing GC, and
//! deferred reference counting (`zct`). A word that merely looks like a box
//! address can only keep a box alive; boxes never move.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

/// The highest stack address that can hold program values.
static STACK_TOP: AtomicUsize = AtomicUsize::new(0);

/// A stack word this far past a box's address (its header, or its value
/// just after, whatever the value's alignment) refers to the box.
pub const SLACK: usize = 48;

/// Record where the stack starts (an address in a frame that outlives
/// every frame holding program values).
pub fn set_top(addr: usize) {
    if STACK_TOP.load(Relaxed) < addr {
        STACK_TOP.store(addr, Relaxed);
    }
}

/// Every word on the stack from the caller's frame up to the recorded top,
/// callee-saved registers included (stored into this frame first), sorted
/// and deduplicated.
pub fn words() -> Vec<usize> {
    words_in(0, usize::MAX)
}

/// `words`, keeping only those in `lo..hi` (the addresses of interest).
#[inline(never)]
pub fn words_in(lo: usize, hi: usize) -> Vec<usize> {
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
    let bottom = regs.as_ptr() as usize;
    let top = STACK_TOP.load(Relaxed);
    let mut out = Vec::new();
    let mut p = bottom;
    while p + 8 <= top {
        // SAFETY: between this frame and the recorded top: mapped stack.
        let w = unsafe { std::ptr::read_volatile(p as *const usize) };
        if w >= lo && w < hi {
            out.push(w);
        }
        p += 8;
    }
    std::hint::black_box(&regs);
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether sorted `words` refer to the box at `a`.
#[inline]
pub fn refers(words: &[usize], a: usize) -> bool {
    let i = words.partition_point(|&w| w < a);
    words.get(i).is_some_and(|&w| w < a + SLACK)
}
