//! `Trace` for every heap type: which `Gc` pointers each value holds.
//!
//! Used by the cycle collector (reference-counting build) and the tracing GC.
//! A `RefCell` that is mutably borrowed at a safepoint is skipped: for the
//! cycle collector that is safe (its children merely look externally
//! referenced); the tracing GC never collects while such a borrow exists.
//! A skipped cell is reported (`heap::opaque`): the ownership check, which
//! spans safepoints, needs to know when a trace was incomplete.

use std::cell::RefCell;

use super::heap::{opaque, Trace, Tracer};
use super::regexp::RegExp;
use super::task::{Accessor, Bound, CoState, Combinator, Coroutine, Job, NativeIter, Promise, PromiseState, Reaction, Symbol};
use super::*;

impl Trace for Value {
    #[inline]
    fn trace(&self, t: &mut Tracer) {
        // Every `Gc` variant's payload is its box (see `Value::gc_header`):
        // one tag test, no match.
        if let Some(h) = self.gc_header() {
            (t.visit)(h);
        } else if let Value::Class(c) = self {
            trace_class(c, t);
        }
    }
}

/// A function's or array's own properties object.
fn trace_props(p: &RefCell<Option<Gc<Object>>>, t: &mut Tracer) {
    match p.try_borrow() {
        Ok(p) => {
            if let Some(o) = &*p {
                t.edge(o);
            }
        }
        Err(_) => opaque(),
    }
}

/// Classes live as long as the program; their shapes may hold prototypes.
fn trace_class(c: &ClassRt, t: &mut Tracer) {
    trace_shape(&c.shape, t);
}

pub fn trace_shape(s: &Shape, t: &mut Tracer) {
    if !t.shared {
        return;
    }
    if let Some(p) = &s.proto {
        t.edge(p);
    }
    if let Some(c) = &s.ctor {
        c.trace(t);
    }
}

impl Trace for i128 {
    const ACYCLIC: bool = true;
    fn trace(&self, _: &mut Tracer) {}
}

impl Trace for JsStr {
    const ACYCLIC: bool = true;
    fn trace(&self, _: &mut Tracer) {}
}

impl Trace for RegExp {
    const ACYCLIC: bool = true;
    fn trace(&self, _: &mut Tracer) {}
}

impl Trace for Symbol {
    const ACYCLIC: bool = true;
    fn trace(&self, _: &mut Tracer) {}
}

impl Trace for RefCell<Value> {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(v) = self.try_borrow() {
            v.trace(t);
        } else {
            opaque();
        }
    }
}

impl Trace for Object {
    fn tail_bytes(&self) -> usize {
        self.inline_slots().len() * size_of::<Value>()
    }
    fn trace(&self, t: &mut Tracer) {
        // Borrowed: cloning the `Rc` would cost a count update per object
        // per collector pass (which skips shapes anyway).
        trace_shape(self.shape_ref(), t);
        for v in self.inline_slots().iter().chain(self.spilled()) {
            v.trace(t);
        }
    }
}

impl Trace for Array {
    fn trace(&self, t: &mut Tracer) {
        match self.store.try_borrow() {
            Ok(store) => {
                if let ArrStore::Val(vs) = &*store {
                    for v in vs {
                        v.trace(t);
                    }
                }
            }
            Err(_) => opaque(),
        }
        trace_props(&self.props, t);
    }
}

impl Trace for Closure {
    fn trace(&self, t: &mut Tracer) {
        for c in self.caps.iter() {
            t.edge(c);
        }
        trace_props(&self.props, t);
    }
}

impl Trace for MapObj {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(m) = self.try_borrow() {
            for (k, v) in m.iter() {
                k.0.trace(t);
                v.trace(t);
            }
        } else {
            opaque();
        }
    }
}

impl Trace for SetObj {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(s) = self.try_borrow() {
            for k in s.iter() {
                k.0.trace(t);
            }
        } else {
            opaque();
        }
    }
}

impl Trace for Bound {
    fn trace(&self, t: &mut Tracer) {
        self.target.trace(t);
        self.this.trace(t);
        for a in &self.args {
            a.trace(t);
        }
    }
}

impl Trace for NativeIter {
    fn trace(&self, t: &mut Tracer) {
        for v in &self.items {
            v.trace(t);
        }
    }
}

impl Trace for Accessor {
    fn trace(&self, t: &mut Tracer) {
        self.get.trace(t);
        self.set.trace(t);
    }
}

impl Trace for Coroutine {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(st) = self.st.try_borrow() {
            trace_co_state(&st, t);
        } else {
            opaque();
        }
    }
}

fn trace_co_state(st: &CoState, t: &mut Tracer) {
    if let Some(c) = &st.closure {
        t.edge(c);
    }
    for v in &st.values {
        v.trace(t);
    }
    if let Some(p) = &st.promise {
        t.edge(p);
    }
    for (_, v, p) in &st.queue {
        v.trace(t);
        t.edge(p);
    }
}

impl Trace for Promise {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(st) = self.state.try_borrow() {
            match &*st {
                PromiseState::Pending(rs) => {
                    for r in rs {
                        r.trace(t);
                    }
                }
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => v.trace(t),
            }
        } else {
            opaque();
        }
    }
}

impl Trace for Reaction {
    fn trace(&self, t: &mut Tracer) {
        match self {
            Reaction::Then { on_fulfilled, on_rejected, result } => {
                on_fulfilled.trace(t);
                on_rejected.trace(t);
                if let Some(p) = result {
                    t.edge(p);
                }
            }
            Reaction::Resume(c) => t.edge(c),
            Reaction::Combine { agg, .. } if t.shared => agg.trace(t),
            Reaction::Combine { .. } => {}
            Reaction::Finally { f, result } => {
                f.trace(t);
                t.edge(result);
            }
        }
    }
}

impl Trace for Combinator {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(vs) = self.values.try_borrow() {
            for v in vs.iter() {
                v.trace(t);
            }
        } else {
            opaque();
        }
        t.edge(&self.result);
    }
}

impl Trace for Job {
    fn trace(&self, t: &mut Tracer) {
        match self {
            Job::React { reaction, value, .. } => {
                reaction.trace(t);
                value.trace(t);
            }
            Job::ResolveThenable { promise, thenable, then } => {
                t.edge(promise);
                thenable.trace(t);
                then.trace(t);
            }
            Job::Call { f, args } => {
                f.trace(t);
                for a in args {
                    a.trace(t);
                }
            }
        }
    }
}
