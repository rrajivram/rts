//! Per-instruction profile (`--features profile`, then `rts --profile`), after
//! rjs's verbose-mode statistics: for every instruction kind, how many times
//! it ran, how much time it took, how many objects it allocated and how many
//! reference-count updates it made.
//!
//! Counts and allocations are exact. Time is sampled: rjs times every
//! instruction with a clock, but most rts instructions take 1–3 ns, well
//! under a clock read (~20 ns), so per-instruction timing measures the clock.
//! Instead each dispatch stores the instruction's kind in `CURRENT`, and a
//! sampler thread reads it every 20 µs (about 50,000 samples a second, on
//! another core) and charges the sample to that kind. The VM runs on one
//! thread, so wall-clock samples measure its CPU time. Time spent in natives, allocation and freeing is charged
//! to the instruction that caused it; samples outside the interpreter (start-up,
//! compilation, the event loop between jobs) go to `<outside>`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::bytecode::{INSTRUCTION_KINDS, INSTRUCTION_NAMES};
use crate::runtime::heap::STATS;

/// Kind of the instruction running now (`OUTSIDE` when none).
pub static CURRENT: AtomicUsize = AtomicUsize::new(OUTSIDE);
const OUTSIDE: usize = INSTRUCTION_KINDS;
static SAMPLES: [AtomicU64; INSTRUCTION_KINDS + 1] = [const { AtomicU64::new(0) }; INSTRUCTION_KINDS + 1];
const INTERVAL: Duration = Duration::from_micros(20);
static RUNNING: AtomicBool = AtomicBool::new(false);
static TOTAL_SAMPLES: AtomicU64 = AtomicU64::new(0);

pub struct Profile {
    count: Vec<u64>,
    allocs: Vec<u64>,
    rc: Vec<u64>,
    last: usize,
    last_allocs: u64,
    last_rc: u64,
    start: Instant,
}

impl Profile {
    /// Start profiling: install the sampling timer.
    pub fn start() -> Box<Profile> {
        RUNNING.store(true, Relaxed);
        std::thread::spawn(|| {
            let mut next = Instant::now() + INTERVAL;
            while RUNNING.load(Relaxed) {
                if Instant::now() >= next {
                    SAMPLES[CURRENT.load(Relaxed).min(OUTSIDE)].fetch_add(1, Relaxed);
                    TOTAL_SAMPLES.fetch_add(1, Relaxed);
                    next += INTERVAL;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        Box::new(Profile {
            count: vec![0; INSTRUCTION_KINDS],
            allocs: vec![0; INSTRUCTION_KINDS],
            rc: vec![0; INSTRUCTION_KINDS],
            last: OUTSIDE,
            last_allocs: STATS.allocated.get(),
            last_rc: STATS.rc_ops.get(),
            start: Instant::now(),
        })
    }

    /// Instruction `kind` is about to run.
    #[inline(always)]
    pub fn tick(&mut self, kind: usize) {
        CURRENT.store(kind, Relaxed);
        let a = STATS.allocated.get();
        let r = STATS.rc_ops.get();
        if self.last != OUTSIDE {
            self.allocs[self.last] += a - self.last_allocs;
            self.rc[self.last] += r - self.last_rc;
        }
        self.last_allocs = a;
        self.last_rc = r;
        self.last = kind;
        self.count[kind] += 1;
    }

    /// (name, count, sampled nanoseconds, allocations, reference-count
    /// updates), most time first; the last row is time outside the interpreter.
    pub fn rows(&mut self) -> Vec<(&'static str, u64, u64, u64, u64)> {
        if self.last != OUTSIDE {
            self.allocs[self.last] += STATS.allocated.get() - self.last_allocs;
            self.rc[self.last] += STATS.rc_ops.get() - self.last_rc;
            self.last = OUTSIDE;
        }
        // Scale samples to the wall time they cover.
        let wall = self.start.elapsed().as_nanos() as f64;
        let total = TOTAL_SAMPLES.load(Relaxed).max(1) as f64;
        let ns = |k: usize| (SAMPLES[k].load(Relaxed) as f64 / total * wall) as u64;
        let mut rows: Vec<_> = (0..INSTRUCTION_KINDS)
            .filter(|&k| self.count[k] > 0)
            .map(|k| (INSTRUCTION_NAMES[k], self.count[k], ns(k), self.allocs[k], self.rc[k]))
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        rows.push(("<outside>", 0, ns(OUTSIDE), 0, 0));
        rows
    }

    /// Print the profile to stderr (and as JSON to `RTS_PROFILE_JSON`, if set).
    pub fn report(&mut self) {
        RUNNING.store(false, Relaxed);
        CURRENT.store(OUTSIDE, Relaxed);
        let rows = self.rows();
        let total: u64 = rows.iter().map(|r| r.2).sum();
        let count: u64 = rows.iter().map(|r| r.1).sum();
        let wall = self.start.elapsed().as_nanos() as u64;
        eprintln!(
            "rts instruction profile: {count} instructions, {} samples (every {} µs), {:.1} ms wall",
            TOTAL_SAMPLES.load(Relaxed),
            INTERVAL.as_micros(),
            wall as f64 / 1e6
        );
        eprintln!("{:<24} {:>13} {:>9} {:>7} {:>8} {:>11} {:>8}", "instruction", "count", "ms", "time%", "ns/op", "allocs", "rc/op");
        for (name, c, ns, a, rc) in &rows {
            let per = |x: f64| if *c > 0 { format!("{:.1}", x / *c as f64) } else { "".into() };
            eprintln!(
                "{:<24} {:>13} {:>9.1} {:>6.1}% {:>8} {:>11} {:>8}",
                name,
                c,
                *ns as f64 / 1e6,
                100.0 * *ns as f64 / total.max(1) as f64,
                per(*ns as f64),
                a,
                per(*rc as f64)
            );
        }
        if let Some(path) = std::env::var_os("RTS_PROFILE_JSON") {
            let body: Vec<String> = rows
                .iter()
                .map(|(n, c, ns, a, rc)| format!("{{\"instruction\":\"{n}\",\"count\":{c},\"ns\":{ns},\"allocs\":{a},\"rc\":{rc}}}"))
                .collect();
            let _ = std::fs::write(path, format!("{{\"wall_ns\":{wall},\"rows\":[{}]}}\n", body.join(",")));
        }
    }
}
