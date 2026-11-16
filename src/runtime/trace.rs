//! `Trace` for every heap type: which `Gc` pointers each value holds.
//!
//! Used by the cycle collector (reference-counting build) and the tracing GC.
//! A `RefCell` that is mutably borrowed at a safepoint is skipped: for the
//! cycle collector that is safe (its children merely look externally
//! referenced); the tracing GC never collects while such a borrow exists.

use std::cell::RefCell;

use super::heap::{Trace, Tracer};
use super::regexp::RegExp;
use super::task::{Accessor, Bound, CoState, Combinator, Coroutine, Job, NativeIter, Promise, PromiseState, Reaction, Symbol};
use super::*;

impl Trace for Value {
    #[inline]
    fn trace(&self, t: &mut Tracer) {
        match self {
            Value::Undef | Value::Null | Value::Bool(_) | Value::Num(_) | Value::Native(_) => {}
            Value::BigInt(g) => t.edge(g),
            Value::Str(g) => t.edge(g),
            Value::Arr(g) => t.edge(g),
            Value::Obj(g) => t.edge(g),
            Value::Func(g) => t.edge(g),
            Value::Cell(g) => t.edge(g),
            Value::Class(c) => trace_class(c, t),
            Value::Map(g) => t.edge(g),
            Value::Set(g) => t.edge(g),
            Value::Bound(g) => t.edge(g),
            Value::Symbol(g) => t.edge(g),
            Value::Gen(g) => t.edge(g),
            Value::Promise(g) => t.edge(g),
            Value::Iter(g) => t.edge(g),
            Value::Regex(g) => t.edge(g),
            Value::Accessor(g) => t.edge(g),
        }
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
        }
    }
}

impl Trace for Object {
    fn tail_bytes(&self) -> usize {
        self.inline_slots().len() * size_of::<Value>()
    }
    fn trace(&self, t: &mut Tracer) {
        trace_shape(&self.shape(), t);
        for v in self.inline_slots().iter().chain(self.spilled()) {
            v.trace(t);
        }
        if let Some(c) = &self.class {
            trace_class(c, t);
        }
    }
}

impl Trace for Array {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(store) = self.store.try_borrow()
            && let ArrStore::Val(vs) = &*store
        {
            for v in vs {
                v.trace(t);
            }
        }
        if let Ok(p) = self.props.try_borrow()
            && let Some(o) = &*p
        {
            t.edge(o);
        }
    }
}

impl Trace for Closure {
    fn trace(&self, t: &mut Tracer) {
        for c in self.caps.iter() {
            t.edge(c);
        }
        if let Ok(p) = self.props.try_borrow()
            && let Some(o) = &*p
        {
            t.edge(o);
        }
    }
}

impl Trace for MapObj {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(m) = self.try_borrow() {
            for (k, v) in m.iter() {
                k.0.trace(t);
                v.trace(t);
            }
        }
    }
}

impl Trace for SetObj {
    fn trace(&self, t: &mut Tracer) {
        if let Ok(s) = self.try_borrow() {
            for k in s.iter() {
                k.0.trace(t);
            }
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
