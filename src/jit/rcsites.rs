//! Profile builds: compiled code's count updates by function and source line.

use super::*;

/// Profile builds: (function, lower.rs line, increment) of each count-update
/// site in compiled code, and how often each ran.
#[cfg(feature = "profile")]
pub(super) const RC_SITE_MAX: usize = 1 << 16;
/// Profile builds: inline count updates of boxes in scope arenas.
#[cfg(feature = "profile")]
pub(super) static mut ARENA_RC: u64 = 0;
#[cfg(feature = "profile")]
pub(super) static mut RC_SITE_COUNTS: [u64; RC_SITE_MAX] = [0; RC_SITE_MAX];
#[cfg(feature = "profile")]
thread_local! {
    pub(super) static RC_SITES: RefCell<Vec<(FuncId, u32, bool)>> = const { RefCell::new(Vec::new()) };
    pub(super) static FN_NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Profile builds: the compiled-code count updates that ran most, by
/// function and by the lower.rs line that emitted them.
#[cfg(feature = "profile")]
pub fn rc_sites_report() {
    let rows: Vec<(u64, String, u32, bool)> = RC_SITES.with(|t| {
        FN_NAMES.with(|names| {
            let names = names.borrow();
            t.borrow()
                .iter()
                .enumerate()
                .take(RC_SITE_MAX)
                .map(|(i, (f, line, inc))| (unsafe { (&raw const RC_SITE_COUNTS).cast::<u64>().add(i).read() }, names.get(*f as usize).cloned().unwrap_or_default(), *line, *inc))
                .filter(|r| r.0 > 0)
                .collect()
        })
    });
    let total: u64 = rows.iter().map(|r| r.0).sum();
    if total == 0 {
        return;
    }
    let mut by_line: FxHashMap<(u32, bool), u64> = FxHashMap::default();
    let mut by_fn: FxHashMap<String, u64> = FxHashMap::default();
    for (n, f, line, inc) in &rows {
        *by_line.entry((*line, *inc)).or_default() += n;
        *by_fn.entry(f.clone()).or_default() += n;
    }
    eprintln!("compiled code count updates: {total} ({} on scope-arena boxes)", unsafe { ARENA_RC });
    let mut v: Vec<_> = by_line.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    for ((line, inc), n) in v.iter().take(15) {
        eprintln!("  {:5.1}%  {} lower.rs:{line}", 100.0 * *n as f64 / total as f64, if *inc { "inc" } else { "dec" });
    }
    let mut v: Vec<_> = by_fn.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (f, n) in v.iter().take(12) {
        eprintln!("  {:5.1}%  in {f}", 100.0 * *n as f64 / total as f64);
    }
    let mut r = rows;
    r.sort_by_key(|x| std::cmp::Reverse(x.0));
    for (n, f, line, inc) in r.iter().take(15) {
        eprintln!("  {:5.1}%  {} lower.rs:{line} in {f}", 100.0 * *n as f64 / total as f64, if *inc { "inc" } else { "dec" });
    }
}
