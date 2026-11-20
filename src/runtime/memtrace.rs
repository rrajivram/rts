//! Memory over time (`RTS_MEM_TRACE=file.json`): a thread samples live heap
//! bytes at a fixed interval (`RTS_MEM_TRACE_US`, default 250 µs), and every
//! pause is logged with its start time and kind: a cycle collection, a
//! tracing-GC collection, a deferred-count settle, or (reference counting)
//! a count reaching zero that frees a structure for 20 µs or more. Written
//! as JSON when the program ends:
//!
//! ```text
//! {"interval_us": 250, "start_bytes": …,
//!  "samples": [[t_us, live_bytes], …], "pauses": [[t_us, ns, kind], …]}
//! ```
//!
//! `start_bytes` is what was live when the program started running
//! (compiled code and all): the program's own memory is above it.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::heap::{LIVE_BYTES, START_BYTES};

struct Trace {
    path: String,
    interval: Duration,
    t0: Instant,
    samples: Arc<Mutex<Vec<(u64, usize)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

static ON: AtomicBool = AtomicBool::new(false);
static mut TRACE: Option<Trace> = None;
static mut PAUSES: Vec<(u64, u64, Kind)> = Vec::new();
/// Depth of nested `release` calls while tracing.
static mut DEPTH: u32 = 0;

/// What paused the program.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Cycles,
    Gc,
    Settle,
    Free,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Cycles => "cycles",
            Kind::Gc => "gc",
            Kind::Settle => "settle",
            Kind::Free => "free",
        }
    }
}

#[inline(always)]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// `release(p)`, timed when outermost: a long one is a pause (freeing a
/// large structure whose last reference went away).
pub fn timed_release(p: std::ptr::NonNull<super::heap::Header>, f: fn(std::ptr::NonNull<super::heap::Header>)) {
    unsafe {
        DEPTH += 1;
        if DEPTH > 1 {
            f(p);
            DEPTH -= 1;
            return;
        }
    }
    let start = Instant::now();
    f(p);
    unsafe { DEPTH -= 1 };
    let d = start.elapsed();
    if d >= Duration::from_micros(20) {
        pause(d, Kind::Free);
    }
}
/// A collector pause starts or ends: frees inside it are part of it.
pub fn nest(f: impl FnOnce()) {
    unsafe { DEPTH += 1 };
    f();
    unsafe { DEPTH -= 1 };
}

/// Samples taken (a cheap check that the sampler ran).
static TAKEN: AtomicUsize = AtomicUsize::new(0);

#[allow(static_mut_refs)]
fn trace() -> Option<&'static mut Trace> {
    unsafe { TRACE.as_mut() }
}

/// Start sampling if `RTS_MEM_TRACE` names a file.
pub fn start_from_env() {
    let Ok(path) = std::env::var("RTS_MEM_TRACE") else { return };
    let us = std::env::var("RTS_MEM_TRACE_US").ok().and_then(|v| v.parse().ok()).unwrap_or(250u64);
    let interval = Duration::from_micros(us.max(10));
    let samples = Arc::new(Mutex::new(Vec::with_capacity(1 << 16)));
    let stop = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let thread = {
        let (samples, stop) = (samples.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Relaxed) {
                let t = t0.elapsed().as_micros() as u64;
                samples.lock().unwrap().push((t, LIVE_BYTES.load(Relaxed)));
                TAKEN.fetch_add(1, Relaxed);
                std::thread::sleep(interval);
            }
        })
    };
    unsafe { TRACE = Some(Trace { path, interval, t0, samples, stop, thread: Some(thread) }) };
    ON.store(true, Relaxed);
}

/// A collector pause of `d` just ended.
#[inline]
pub fn pause(d: Duration, kind: Kind) {
    if !ON.load(Relaxed) {
        return;
    }
    if let Some(t) = trace() {
        let end = t.t0.elapsed();
        let at = end.saturating_sub(d).as_micros() as u64;
        #[allow(static_mut_refs)]
        unsafe {
            PAUSES.push((at, d.as_nanos() as u64, kind))
        };
    }
}

/// Stop sampling and write the file.
#[allow(static_mut_refs)]
pub fn finish() {
    let Some(t) = trace() else { return };
    ON.store(false, Relaxed);
    t.stop.store(true, Relaxed);
    if let Some(h) = t.thread.take() {
        let _ = h.join();
    }
    let samples = t.samples.lock().unwrap();
    let mut s = String::with_capacity(samples.len() * 20 + 1024);
    s.push_str(&format!("{{\"interval_us\": {}, \"start_bytes\": {}, \"samples\": [", t.interval.as_micros(), START_BYTES.load(Relaxed)));
    for (i, (at, b)) in samples.iter().enumerate() {
        s.push_str(&format!("{}[{at},{b}]", if i > 0 { "," } else { "" }));
    }
    s.push_str("], \"pauses\": [");
    for (i, (at, ns, k)) in unsafe { PAUSES.iter() }.enumerate() {
        s.push_str(&format!("{}[{at},{ns},\"{}\"]", if i > 0 { "," } else { "" }, k.name()));
    }
    s.push_str("]}\n");
    if let Err(e) = std::fs::write(&t.path, s) {
        eprintln!("rts: RTS_MEM_TRACE {}: {e}", t.path);
    }
}
