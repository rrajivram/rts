//! Generators, async functions, promises and the event loop.
//!
//! A generator or async function runs in an ordinary frame. Suspending it
//! (`CreateGenerator`, `Yield`, `Await`) moves the frame's slots into its
//! `Coroutine` and pops the frame; resuming pushes a frame on top of the
//! stack, moves the slots back and runs a nested interpreter loop until the
//! function suspends again or finishes. Typed slots stay unboxed throughout.
//!
//! Promises follow the ECMAScript job model, so callbacks run in the same
//! order as in Node: reactions are queued as microtasks, `process.nextTick`
//! callbacks run before them, and timers run after the microtask queue drains.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::*;
use crate::runtime::task::*;

/// A pending `setTimeout` / `setInterval` / `setImmediate`.
pub(super) struct Timer {
    pub id: u32,
    /// Milliseconds since the VM started.
    pub due: f64,
    pub seq: u64,
    pub f: Value,
    pub args: Vec<Value>,
    pub repeat: Option<f64>,
    pub immediate: bool,
}

#[derive(Default)]
pub(super) struct EventLoop {
    pub microtasks: VecDeque<Job>,
    pub ticks: VecDeque<Job>,
    pub timers: Vec<Timer>,
    pub seq: u64,
    pub next_id: u32,
    /// Rejected promises that had no handler when they were rejected.
    pub rejected: Vec<Rc<Promise>>,
}

thread_local! {
    static ITER_SHAPE: Rc<Shape> = Shape::new(vec!["value".into(), "done".into()]);
}

/// `{ value, done }`
pub fn iter_result(value: Value, done: bool) -> Value {
    let sh = ITER_SHAPE.with(|s| s.clone());
    Value::Obj(Object::new(sh, vec![value, Value::Bool(done)], None))
}

impl Vm {
    // ───────────── frames ─────────────

    /// Hand a finished (or suspended) frame's result to whoever called it.
    pub(super) fn deliver(&mut self, ret: Ret, val: Value) -> Result<(), Value> {
        match ret {
            Ret::ToSlot(k, d) => {
                let top = self.frames.last().unwrap();
                let (sb, vb) = (top.scalar_base, top.value_base);
                self.store(k, d, sb, vb, val)
            }
            Ret::Discard => Ok(()),
            Ret::Native => {
                self.native_ret = val;
                Ok(())
            }
        }
    }

    /// Pop the top frame, moving its slots and `try` handlers into its
    /// coroutine. `pc` is where it will continue.
    pub(super) fn suspend_top(&mut self, pc: usize) -> Frame {
        let fr = self.frames.pop().unwrap();
        let co = self.pop_co(self.frames.len()).expect("suspending a frame that is not a coroutine");
        let f = fr.func.get();
        let fi = self.frames.len();
        let mut st = co.st.borrow_mut();
        st.pc = pc;
        st.scalars.clear();
        st.scalars.extend_from_slice(&self.scalars[fr.scalar_base..fr.scalar_base + f.scalar_slots]);
        st.values.clear();
        st.values.extend(self.values[fr.value_base..fr.value_base + f.value_slots].iter_mut().map(std::mem::take));
        let mut hs = Vec::new();
        while let Some(&(hf, catch, exc)) = self.handlers.last()
            && hf == fi
        {
            self.handlers.pop();
            hs.push((catch, exc.0));
        }
        hs.reverse();
        st.handlers = hs;
        st.status = CoStatus::Suspended;
        drop(st);
        fr
    }

    /// Run a suspended coroutine until it suspends again or finishes. Returns
    /// what it yielded or returned (check the status to tell which).
    pub fn resume(&mut self, co: &Rc<Coroutine>, mode: Resume, val: Value) -> Result<Value, Value> {
        let (fid, closure, pc, scalars, values, handlers, was) = {
            let mut st = co.st.borrow_mut();
            let was = st.status;
            st.status = CoStatus::Running;
            st.awaiting = false;
            (
                st.func,
                st.closure.clone(),
                st.pc,
                std::mem::take(&mut st.scalars),
                std::mem::take(&mut st.values),
                std::mem::take(&mut st.handlers),
                was,
            )
        };
        let func = self.prog.funcs[fid as usize].clone();
        if self.frames.len() >= MAX_DEPTH {
            co.st.borrow_mut().status = CoStatus::Done;
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        let (sb, vb) = match self.frames.last() {
            Some(top) => (top.scalar_base + top.func.get().scalar_slots, top.value_base + top.func.get().value_slots),
            None => (0, 0),
        };
        if let Err(e) = self.ensure(sb, vb, &func) {
            co.st.borrow_mut().status = CoStatus::Done;
            return Err(e);
        }
        self.scalars[sb..sb + scalars.len()].copy_from_slice(&scalars);
        for (i, v) in values.into_iter().enumerate() {
            self.values[vb + i] = v;
        }
        let fi = self.frames.len();
        for (catch, exc) in handlers {
            self.handlers.push((fi, catch, ValSlot(exc)));
        }
        if was == CoStatus::Suspended {
            match func.code[pc - 1] {
                Instruction::Yield { received, mode: m, .. } | Instruction::Await { received, mode: m, .. } => {
                    self.values[vb + received.index()] = val;
                    self.scalars[sb + m.index()] = bits(mode as u8 as f64);
                }
                _ => unreachable!("resuming after a non-suspending instruction"),
            }
        }
        self.frames.push(Frame {
            func: FnRef::new(&func),
            pc,
            scalar_base: sb,
            value_base: vb,
            closure,
            ret: Ret::Native,
        });
        self.co_frames.push((fi, co.clone()));
        let r = self.run(fi);
        let mut st = co.st.borrow_mut();
        match r {
            Ok(()) => {
                if st.status == CoStatus::Running {
                    st.status = CoStatus::Done;
                }
                Ok(std::mem::take(&mut self.native_ret))
            }
            Err(e) => {
                st.status = CoStatus::Done;
                Err(e)
            }
        }
    }

    /// A coroutine's body returned (`Ok`) or threw (`Err`, async kinds only).
    /// Settles its promise; returns what the frame's caller receives.
    pub(super) fn complete_coroutine(&mut self, co: &Rc<Coroutine>, outcome: Result<Value, Value>) -> Value {
        let promise = {
            let mut st = co.st.borrow_mut();
            st.status = CoStatus::Done;
            st.promise.clone()
        };
        match co.kind {
            CoKind::Generator => outcome.unwrap_or(Value::Undef),
            CoKind::Async => {
                let p = promise.expect("async function without a promise");
                match outcome {
                    Ok(v) => self.resolve_promise(&p, v),
                    Err(e) => self.reject_promise(&p, e),
                }
                Value::Promise(p)
            }
            CoKind::AsyncGenerator => {
                let req = co.st.borrow_mut().queue.pop_front();
                if let Some((_, _, p)) = req {
                    match outcome {
                        Ok(v) => self.resolve_promise(&p, iter_result(v, true)),
                        Err(e) => self.reject_promise(&p, e),
                    }
                }
                Value::Undef
            }
        }
    }

    // ───────────── generators ─────────────

    /// `gen.next(v)` / `gen.throw(v)` / `gen.return(v)`: (value, done).
    pub fn generator_step(&mut self, co: &Rc<Coroutine>, mode: Resume, val: Value) -> Result<(Value, bool), Value> {
        let status = co.st.borrow().status;
        let finished = |vm: &mut Vm, val: Value| match mode {
            Resume::Next => Ok((Value::Undef, true)),
            Resume::Return => Ok((val, true)),
            Resume::Throw => {
                let _ = vm;
                Err(val)
            }
        };
        match status {
            CoStatus::Running => Err(self.type_error("Generator is already running".into())),
            CoStatus::Done => finished(self, val),
            CoStatus::Start if mode != Resume::Next => {
                co.st.borrow_mut().status = CoStatus::Done;
                finished(self, val)
            }
            _ => {
                let v = self.resume(co, mode, val)?;
                let done = co.st.borrow().status == CoStatus::Done;
                Ok((v, done))
            }
        }
    }

    // ───────────── async generators ─────────────

    /// `next`/`throw`/`return` on an async generator: queue the request and
    /// return its promise.
    pub fn async_generator_request(&mut self, co: &Rc<Coroutine>, mode: Resume, val: Value) -> Value {
        let p = Promise::new();
        co.st.borrow_mut().queue.push_back((mode, val, p.clone()));
        self.async_generator_drive(co);
        Value::Promise(p)
    }

    /// Serve queued requests while the generator is idle.
    pub(super) fn async_generator_drive(&mut self, co: &Rc<Coroutine>) {
        loop {
            let (status, awaiting, front) = {
                let st = co.st.borrow();
                (st.status, st.awaiting, st.queue.front().map(|(m, v, p)| (*m, v.clone(), p.clone())))
            };
            if status == CoStatus::Running || awaiting {
                return;
            }
            let Some((mode, val, p)) = front else { return };
            if status == CoStatus::Done || (status == CoStatus::Start && mode != Resume::Next) {
                let mut st = co.st.borrow_mut();
                st.status = CoStatus::Done;
                st.queue.pop_front();
                drop(st);
                match mode {
                    Resume::Next => self.resolve_promise(&p, iter_result(Value::Undef, true)),
                    Resume::Return => self.resolve_promise(&p, iter_result(val, true)),
                    Resume::Throw => self.reject_promise(&p, val),
                }
                continue;
            }
            // Errors inside the body reject the request's promise (see unwind).
            let _ = self.resume(co, mode, val);
        }
    }

    // ───────────── iteration ─────────────

    /// `value[Symbol.iterator]()`
    pub fn get_iterator(&mut self, v: &Value) -> Result<Value, Value> {
        match v {
            Value::Gen(g) if g.kind == CoKind::Generator => Ok(v.clone()),
            Value::Arr(_) | Value::Str(_) | Value::Map(_) | Value::Set(_) | Value::Iter(_) => {
                let items = match v {
                    Value::Iter(it) => it.rest(),
                    v => natives::iterate(self, v)?,
                };
                Ok(Value::Iter(NativeIter::new("Array Iterator", items)))
            }
            Value::Obj(_) => {
                let f = self.get_property(v, &well_known("iterator"))?;
                if !is_callable(&f) {
                    return Err(self.type_error(format!("{} is not iterable", describe_short(v))));
                }
                let it = self.call(&f, v.clone(), vec![])?;
                if !matches!(it, Value::Obj(_) | Value::Gen(_) | Value::Iter(_)) {
                    return Err(self.type_error("Result of the Symbol.iterator method is not an object".into()));
                }
                Ok(it)
            }
            v => Err(self.type_error(format!("{} is not iterable", describe_short(v)))),
        }
    }

    /// The next value of an iterator, or None when it is done.
    pub fn iterator_step(&mut self, it: &Value) -> Result<Option<Value>, Value> {
        match it {
            Value::Gen(g) if g.kind == CoKind::Generator => {
                let (v, done) = self.generator_step(g, Resume::Next, Value::Undef)?;
                Ok((!done).then_some(v))
            }
            Value::Iter(i) => Ok(i.next()),
            _ => {
                let r = self.call_method(it, "next", vec![])?;
                self.unpack_result(&r)
            }
        }
    }

    /// Read `{ value, done }`: Some(value) unless done.
    pub fn unpack_result(&mut self, r: &Value) -> Result<Option<Value>, Value> {
        if !matches!(r, Value::Obj(_)) {
            return Err(self.type_error(format!("Iterator result {} is not an object", describe_short(r))));
        }
        let done = self.get_property(r, "done")?.truthy();
        if done {
            return Ok(None);
        }
        Ok(Some(self.get_property(r, "value")?))
    }

    /// Leaving a loop over `it` early.
    pub fn iterator_close(&mut self, it: &Value) -> Result<(), Value> {
        match it {
            Value::Gen(g) if g.kind == CoKind::Generator => {
                self.generator_step(g, Resume::Return, Value::Undef)?;
            }
            Value::Obj(_) => {
                let f = self.get_property(it, "return")?;
                if is_callable(&f) {
                    self.call(&f, it.clone(), vec![])?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// One step of `yield*`: pass a next/throw/return on to `it`.
    pub fn iterator_resume(&mut self, it: &Value, mode: Resume, val: Value) -> Result<(Value, bool), Value> {
        if let Value::Gen(g) = it
            && g.kind == CoKind::Generator
        {
            return self.generator_step(g, mode, val);
        }
        let name = match mode {
            Resume::Next => "next",
            Resume::Throw => "throw",
            Resume::Return => "return",
        };
        if let Value::Iter(i) = it {
            return Ok(match mode {
                Resume::Next => match i.next() {
                    Some(v) => (v, false),
                    None => (Value::Undef, true),
                },
                Resume::Return => (val, true),
                Resume::Throw => return Err(val),
            });
        }
        let f = self.get_property(it, name)?;
        if !is_callable(&f) {
            return match mode {
                Resume::Return => Ok((val, true)),
                Resume::Throw => {
                    self.iterator_close(it)?;
                    Err(self.type_error("The iterator does not provide a 'throw' method".into()))
                }
                Resume::Next => Err(self.type_error(format!("{} is not a function", name))),
            };
        }
        let r = self.call(&f, it.clone(), if mode == Resume::Next && matches!(val, Value::Undef) { vec![] } else { vec![val] })?;
        match self.unpack_result(&r)? {
            Some(v) => Ok((v, false)),
            None => Ok((self.get_property(&r, "value")?, true)),
        }
    }

    /// `for await`: the async iterator of `v`, or (with `true`) its sync iterator.
    pub fn get_async_iterator(&mut self, v: &Value) -> Result<(Value, bool), Value> {
        match v {
            Value::Gen(g) if g.kind == CoKind::AsyncGenerator => Ok((v.clone(), false)),
            Value::Obj(_) => {
                let f = self.get_property(v, &well_known("asyncIterator"))?;
                if is_callable(&f) {
                    let it = self.call(&f, v.clone(), vec![])?;
                    return Ok((it, false));
                }
                Ok((self.get_iterator(v)?, true))
            }
            v => Ok((self.get_iterator(v)?, true)),
        }
    }

    /// `it.next()` as a raw result (a promise for async iterators).
    pub fn iterator_next_result(&mut self, it: &Value) -> Result<Value, Value> {
        match it {
            Value::Gen(g) if g.kind == CoKind::AsyncGenerator => Ok(self.async_generator_request(g, Resume::Next, Value::Undef)),
            Value::Gen(g) => {
                let (v, done) = self.generator_step(g, Resume::Next, Value::Undef)?;
                Ok(iter_result(v, done))
            }
            Value::Iter(i) => Ok(match i.next() {
                Some(v) => iter_result(v, false),
                None => iter_result(Value::Undef, true),
            }),
            _ => self.call_method(it, "next", vec![]),
        }
    }

    pub fn get_property(&mut self, v: &Value, name: &str) -> Result<Value, Value> {
        let site = PropertySite::new(name);
        self.get_prop_slow(v, &site)
    }

    pub fn call_method(&mut self, recv: &Value, name: &str, args: Vec<Value>) -> Result<Value, Value> {
        let f = self.get_property(recv, name)?;
        if !is_callable(&f) {
            return Err(self.type_error(format!("{}.{name} is not a function", describe_short(recv))));
        }
        self.call(&f, recv.clone(), args)
    }

    // ───────────── promises ─────────────

    pub fn fulfill_promise(&mut self, p: &Rc<Promise>, v: Value) {
        let old = std::mem::replace(&mut *p.state.borrow_mut(), PromiseState::Fulfilled(v.clone()));
        if let PromiseState::Pending(reactions) = old {
            for r in reactions {
                self.lp.microtasks.push_back(Job::React { reaction: r, value: v.clone(), rejected: false });
            }
        }
    }

    pub fn reject_promise(&mut self, p: &Rc<Promise>, e: Value) {
        if !p.is_pending() {
            return;
        }
        let old = std::mem::replace(&mut *p.state.borrow_mut(), PromiseState::Rejected(e.clone()));
        if !p.handled.get() {
            self.lp.rejected.push(p.clone());
        }
        if let PromiseState::Pending(reactions) = old {
            for r in reactions {
                self.lp.microtasks.push_back(Job::React { reaction: r, value: e.clone(), rejected: true });
            }
        }
    }

    /// Resolve `p` with `v`: adopt `v`'s state if it is a promise or thenable.
    pub fn resolve_promise(&mut self, p: &Rc<Promise>, v: Value) {
        if !p.is_pending() {
            return;
        }
        match &v {
            Value::Promise(q) if Rc::ptr_eq(p, q) => {
                let e = self.type_error("Chaining cycle detected for promise #<Promise>".into());
                self.reject_promise(p, e);
            }
            Value::Promise(_) => self.lp.microtasks.push_back(Job::ResolveThenable {
                promise: p.clone(),
                thenable: v.clone(),
                then: Value::Native(Native::PromiseThen),
            }),
            Value::Obj(_) => match self.get_property(&v, "then") {
                Err(e) => self.reject_promise(p, e),
                Ok(then) if is_callable(&then) => self.lp.microtasks.push_back(Job::ResolveThenable {
                    promise: p.clone(),
                    thenable: v.clone(),
                    then,
                }),
                Ok(_) => self.fulfill_promise(p, v),
            },
            _ => self.fulfill_promise(p, v),
        }
    }

    /// PerformPromiseThen.
    pub fn promise_react(&mut self, p: &Rc<Promise>, reaction: Reaction) {
        p.handled.set(true);
        let mut st = p.state.borrow_mut();
        match &mut *st {
            PromiseState::Pending(rs) => rs.push(reaction),
            PromiseState::Fulfilled(v) => {
                let v = v.clone();
                drop(st);
                self.lp.microtasks.push_back(Job::React { reaction, value: v, rejected: false });
            }
            PromiseState::Rejected(e) => {
                let e = e.clone();
                drop(st);
                self.lp.microtasks.push_back(Job::React { reaction, value: e, rejected: true });
            }
        }
    }

    /// PromiseResolve(%Promise%, v): `v` itself if it is a promise.
    pub fn promise_of(&mut self, v: Value) -> Rc<Promise> {
        if let Value::Promise(p) = v {
            return p;
        }
        let p = Promise::new();
        self.resolve_promise(&p, v);
        p
    }

    /// The resolve and reject functions handed to an executor or a thenable.
    pub fn resolving_functions(&mut self, p: &Rc<Promise>) -> (Value, Value) {
        let flag = Value::Cell(Rc::new(std::cell::RefCell::new(Value::Bool(false))));
        let mk = |n: Native| {
            Value::Bound(Rc::new(Bound {
                target: Value::Native(n),
                this: Value::Promise(p.clone()),
                args: vec![flag.clone()],
            }))
        };
        (mk(Native::PromiseResolveFunction), mk(Native::PromiseRejectFunction))
    }

    fn run_job(&mut self, job: Job) -> Result<(), Value> {
        match job {
            Job::Call { f, args } => {
                self.call(&f, Value::Undef, args)?;
            }
            Job::ResolveThenable { promise, thenable, then } => {
                let (res, rej) = self.resolving_functions(&promise);
                if let (Value::Promise(q), Value::Native(Native::PromiseThen)) = (&thenable, &then) {
                    self.promise_react(
                        q,
                        Reaction::Then {
                            on_fulfilled: res,
                            on_rejected: rej,
                            result: None,
                        },
                    );
                } else if let Err(e) = self.call(&then, thenable, vec![res, rej.clone()]) {
                    self.call(&rej, Value::Undef, vec![e])?;
                }
            }
            Job::React { reaction, value, rejected } => match reaction {
                Reaction::Then { on_fulfilled, on_rejected, result } => {
                    let handler = if rejected { on_rejected } else { on_fulfilled };
                    let outcome = if is_callable(&handler) {
                        self.call(&handler, Value::Undef, vec![value])
                    } else if rejected {
                        Err(value)
                    } else {
                        Ok(value)
                    };
                    if let Some(r) = result {
                        match outcome {
                            Ok(v) => self.resolve_promise(&r, v),
                            Err(e) => self.reject_promise(&r, e),
                        }
                    } else {
                        outcome?;
                    }
                }
                Reaction::Resume(co) => {
                    let mode = if rejected { Resume::Throw } else { Resume::Next };
                    let r = self.resume(&co, mode, value);
                    if co.kind == CoKind::AsyncGenerator {
                        self.async_generator_drive(&co);
                    }
                    r?;
                }
                Reaction::Combine { agg, index } => self.combine(&agg, index, value, rejected),
                Reaction::Finally { f, result } => match self.call(&f, Value::Undef, vec![]) {
                    Err(e) => self.reject_promise(&result, e),
                    Ok(r) => {
                        let pr = self.promise_of(r);
                        let thunk = Value::Bound(Rc::new(Bound {
                            target: Value::Native(Native::PromiseThunk),
                            this: value,
                            args: vec![Value::Bool(rejected)],
                        }));
                        self.promise_react(
                            &pr,
                            Reaction::Then {
                                on_fulfilled: thunk,
                                on_rejected: Value::Undef,
                                result: Some(result),
                            },
                        );
                    }
                },
            },
        }
        Ok(())
    }

    fn combine(&mut self, agg: &Rc<Combinator>, index: usize, value: Value, rejected: bool) {
        if !agg.result.is_pending() {
            return;
        }
        match (agg.kind, rejected) {
            (CombineKind::All, true) => return self.reject_promise(&agg.result, value),
            (CombineKind::Any, false) => return self.resolve_promise(&agg.result, value),
            (CombineKind::AllSettled, _) => {
                let (status, key) = if rejected { ("rejected", "reason") } else { ("fulfilled", "value") };
                let o = Object::new(Shape::new(vec!["status".into(), key.into()]), vec![Value::str(status), value], None);
                agg.values.borrow_mut()[index] = Value::Obj(o);
            }
            _ => agg.values.borrow_mut()[index] = value,
        }
        agg.remaining.set(agg.remaining.get() - 1);
        if agg.remaining.get() == 0 {
            let vals = std::mem::take(&mut *agg.values.borrow_mut());
            let arr = Value::Arr(natives::make_array(vals));
            if agg.kind == CombineKind::Any {
                let e = self.aggregate_error(arr);
                self.reject_promise(&agg.result, e);
            } else {
                self.resolve_promise(&agg.result, arr);
            }
        }
    }

    fn aggregate_error(&mut self, errors: Value) -> Value {
        let e = self.make_error(self.prog.error_class, "All promises were rejected".into());
        if let Value::Obj(o) = &e {
            o.set(&"name".into(), Value::str("AggregateError"));
            o.set(&"errors".into(), errors);
        }
        e
    }

    /// `Promise.all` and friends over an iterable.
    pub fn promise_combinator(&mut self, kind: CombineKind, iterable: &Value) -> Result<Value, Value> {
        let items = natives::iterate(self, iterable)?;
        let result = Promise::new();
        let n = items.len();
        let agg = Rc::new(Combinator {
            kind,
            values: std::cell::RefCell::new(vec![Value::Undef; n]),
            remaining: std::cell::Cell::new(n),
            result: result.clone(),
        });
        if n == 0 {
            match kind {
                CombineKind::Any => {
                    let e = self.aggregate_error(Value::Arr(natives::make_array(vec![])));
                    self.reject_promise(&result, e);
                }
                _ => self.resolve_promise(&result, Value::Arr(natives::make_array(vec![]))),
            }
        }
        for (index, item) in items.into_iter().enumerate() {
            let p = self.promise_of(item);
            self.promise_react(&p, Reaction::Combine { agg: agg.clone(), index });
        }
        Ok(Value::Promise(result))
    }

    pub fn promise_race(&mut self, iterable: &Value) -> Result<Value, Value> {
        let items = natives::iterate(self, iterable)?;
        let result = Promise::new();
        for item in items {
            let p = self.promise_of(item);
            let (res, rej) = self.resolving_functions(&result);
            self.promise_react(
                &p,
                Reaction::Then {
                    on_fulfilled: res,
                    on_rejected: rej,
                    result: None,
                },
            );
        }
        Ok(Value::Promise(result))
    }

    // ───────────── the event loop ─────────────

    pub fn queue_microtask(&mut self, f: Value, args: Vec<Value>) {
        self.lp.microtasks.push_back(Job::Call { f, args });
    }

    pub fn next_tick(&mut self, f: Value, args: Vec<Value>) {
        self.lp.ticks.push_back(Job::Call { f, args });
    }

    pub fn add_timer(&mut self, f: Value, ms: f64, args: Vec<Value>, repeat: bool, immediate: bool) -> u32 {
        let ms = if ms.is_nan() || ms < 1.0 { 1.0 } else { ms };
        self.lp.next_id += 1;
        self.lp.seq += 1;
        let id = self.lp.next_id;
        let now = self.now_ms();
        self.lp.timers.push(Timer {
            id,
            due: if immediate { now } else { now + ms },
            seq: self.lp.seq,
            f,
            args,
            repeat: repeat.then_some(ms),
            immediate,
        });
        id
    }

    pub fn clear_timer(&mut self, id: u32) {
        self.lp.timers.retain(|t| t.id != id);
    }

    pub fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Drain `process.nextTick` callbacks and microtasks.
    pub fn run_microtasks(&mut self) -> Result<(), Value> {
        loop {
            if let Some(job) = self.lp.ticks.pop_front() {
                self.run_job(job)?;
            } else if let Some(job) = self.lp.microtasks.pop_front() {
                self.run_job(job)?;
            } else {
                break;
            }
        }
        self.check_rejections()
    }

    /// A promise rejected with no handler by the time the microtasks ran out
    /// ends the program, as in Node.
    fn check_rejections(&mut self) -> Result<(), Value> {
        let rejected = std::mem::take(&mut self.lp.rejected);
        for p in rejected {
            if !p.handled.get()
                && let PromiseState::Rejected(e) = &*p.state.borrow()
            {
                return Err(e.clone());
            }
        }
        Ok(())
    }

    /// After the main module: run timers until none are left.
    pub fn run_event_loop(&mut self) -> Result<(), Value> {
        self.run_microtasks()?;
        loop {
            // Timers that are due, earliest first; then immediates.
            let now = self.now_ms();
            let next = self
                .lp
                .timers
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.immediate)
                .min_by(|(_, a), (_, b)| a.due.total_cmp(&b.due).then(a.seq.cmp(&b.seq)))
                .map(|(i, t)| (i, t.due));
            let immediate = self.lp.timers.iter().position(|t| t.immediate);
            let pick = match (next, immediate) {
                (Some((i, due)), _) if due <= now => Some(i),
                (_, Some(i)) => Some(i),
                (Some((i, due)), None) => {
                    self.flush();
                    std::thread::sleep(Duration::from_secs_f64(((due - now) / 1000.0).max(0.0)));
                    Some(i)
                }
                (None, None) => None,
            };
            let Some(i) = pick else { break };
            let t = self.lp.timers.remove(i);
            if let Some(ms) = t.repeat {
                self.lp.seq += 1;
                self.lp.timers.push(Timer {
                    id: t.id,
                    due: self.now_ms() + ms,
                    seq: self.lp.seq,
                    f: t.f.clone(),
                    args: t.args.clone(),
                    repeat: t.repeat,
                    immediate: false,
                });
            }
            self.call(&t.f, Value::Undef, t.args)?;
            self.run_microtasks()?;
        }
        let _ = Instant::now();
        Ok(())
    }
}

pub fn is_callable(v: &Value) -> bool {
    matches!(v, Value::Func(_) | Value::Native(_) | Value::Bound(_) | Value::Class(_))
}

fn describe_short(v: &Value) -> String {
    match v {
        Value::Undef => "undefined".into(),
        Value::Null => "null".into(),
        Value::Obj(_) => "object".into(),
        v => v.type_of().into(),
    }
}

/// Natives for generators, promises, symbols and timers.
pub fn call_task_native(vm: &mut Vm, n: Native, this: Value, args: Vec<Value>) -> Result<Value, Value> {
    use Native::*;
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undef);
    Ok(match n {
        GenNext | GenReturn | GenThrow => {
            let mode = match n {
                GenNext => Resume::Next,
                GenReturn => Resume::Return,
                _ => Resume::Throw,
            };
            match &this {
                Value::Gen(g) if g.kind == CoKind::AsyncGenerator => vm.async_generator_request(g, mode, arg(0)),
                Value::Gen(g) => {
                    let (v, done) = vm.generator_step(g, mode, arg(0))?;
                    iter_result(v, done)
                }
                Value::Iter(i) if n == GenNext => match i.next() {
                    Some(v) => iter_result(v, false),
                    None => iter_result(Value::Undef, true),
                },
                Value::Iter(_) => iter_result(arg(0), true),
                _ => return Err(vm.type_error(format!("{}() called on an incompatible receiver", n.name()))),
            }
        }
        IterSelf => this,
        SymbolFn => {
            let desc = match arg(0) {
                Value::Undef => None,
                v => Some(Rc::<str>::from(&*v.to_js_string().s)),
            };
            vm.lp.seq += 1;
            let key = format!("{SYMBOL_PREFIX}{}:{}", vm.lp.seq, desc.as_deref().unwrap_or(""));
            Value::Symbol(Rc::new(Symbol { description: desc, key: key.into() }))
        }
        SymbolWellKnown => {
            let name = arg(0).to_js_string();
            Value::Symbol(Rc::new(Symbol {
                description: Some(format!("Symbol.{}", name.s).into()),
                key: well_known(&name.s),
            }))
        }
        // `new f(…)` where `f` is only known at run time.
        Construct => vm.construct(&arg(0), args[1.min(args.len())..].to_vec())?,
        // `x instanceof f` where `f` is only known at run time.
        InstanceOfDyn => Value::Bool(vm.instance_of(&arg(0), &arg(1))?),
        IsInstanceOfBuiltin => {
            let v = arg(0);
            Value::Bool(match &*arg(1).to_js_string().s {
                "Promise" => matches!(v, Value::Promise(_)),
                "Map" => matches!(v, Value::Map(_)),
                "Set" => matches!(v, Value::Set(_)),
                "Function" => is_callable(&v),
                "RegExp" => matches!(v, Value::Regex(_)),
                "Object" => !matches!(v, Value::Undef | Value::Null | Value::Bool(_) | Value::Num(_) | Value::Str(_) | Value::BigInt(_) | Value::Symbol(_)),
                _ => false,
            })
        }
        SymbolFor => {
            let k = arg(0).to_js_string();
            Value::Symbol(Rc::new(Symbol {
                description: Some((&*k.s).into()),
                key: format!("{SYMBOL_PREFIX}for:{}", k.s).into(),
            }))
        }
        PromiseNew => {
            let exec = arg(0);
            if !is_callable(&exec) {
                return Err(vm.type_error(format!("Promise resolver {} is not a function", vm.inspect(&exec))));
            }
            let p = Promise::new();
            let (res, rej) = vm.resolving_functions(&p);
            if let Err(e) = vm.call(&exec, Value::Undef, vec![res, rej.clone()]) {
                vm.call(&rej, Value::Undef, vec![e])?;
            }
            Value::Promise(p)
        }
        PromiseResolveFunction | PromiseRejectFunction => {
            // Bound: this = the promise, args = [already-resolved flag, value].
            let Value::Promise(p) = &this else { unreachable!() };
            let Some(Value::Cell(flag)) = args.first() else { unreachable!() };
            if flag.borrow().truthy() {
                return Ok(Value::Undef);
            }
            *flag.borrow_mut() = Value::Bool(true);
            let v = args.get(1).cloned().unwrap_or(Value::Undef);
            if n == PromiseResolveFunction {
                vm.resolve_promise(p, v);
            } else {
                vm.reject_promise(p, v);
            }
            Value::Undef
        }
        PromiseThunk => {
            // Bound: this = the value, args = [rejected].
            if args.first().is_some_and(|r| r.truthy()) {
                return Err(this);
            }
            this
        }
        PromiseThen | PromiseCatch | PromiseFinally => {
            let Value::Promise(p) = &this else {
                return Err(vm.type_error(format!("Method Promise.prototype.{} called on incompatible receiver", n.name())));
            };
            let result = Promise::new();
            let reaction = match n {
                PromiseThen => Reaction::Then {
                    on_fulfilled: arg(0),
                    on_rejected: arg(1),
                    result: Some(result.clone()),
                },
                PromiseCatch => Reaction::Then {
                    on_fulfilled: Value::Undef,
                    on_rejected: arg(0),
                    result: Some(result.clone()),
                },
                _ if !is_callable(&arg(0)) => Reaction::Then {
                    on_fulfilled: Value::Undef,
                    on_rejected: Value::Undef,
                    result: Some(result.clone()),
                },
                _ => Reaction::Finally {
                    f: arg(0),
                    result: result.clone(),
                },
            };
            vm.promise_react(p, reaction);
            Value::Promise(result)
        }
        PromiseResolve => Value::Promise(vm.promise_of(arg(0))),
        PromiseReject => {
            let p = Promise::new();
            vm.reject_promise(&p, arg(0));
            Value::Promise(p)
        }
        PromiseAll => vm.promise_combinator(CombineKind::All, &arg(0))?,
        PromiseAllSettled => vm.promise_combinator(CombineKind::AllSettled, &arg(0))?,
        PromiseAny => vm.promise_combinator(CombineKind::Any, &arg(0))?,
        PromiseRace => vm.promise_race(&arg(0))?,
        PromiseWithResolvers => {
            let p = Promise::new();
            let (res, rej) = vm.resolving_functions(&p);
            let sh = Shape::new(vec!["promise".into(), "resolve".into(), "reject".into()]);
            Value::Obj(Object::new(sh, vec![Value::Promise(p), res, rej], None))
        }
        SetTimeout | SetInterval | SetImmediate => {
            let f = arg(0);
            if !is_callable(&f) {
                return Err(vm.type_error(format!("The \"callback\" argument must be of type function. Received {}", vm.inspect(&f))));
            }
            let (ms, rest) = if n == SetImmediate { (0.0, args.get(1..)) } else { (arg(1).to_number(), args.get(2..)) };
            let rest = rest.map(|r| r.to_vec()).unwrap_or_default();
            Value::Num(vm.add_timer(f, ms, rest, n == SetInterval, n == SetImmediate) as f64)
        }
        ClearTimeout => {
            if let Value::Num(id) = arg(0) {
                vm.clear_timer(id as u32);
            }
            Value::Undef
        }
        QueueMicrotask => {
            vm.queue_microtask(arg(0), vec![]);
            Value::Undef
        }
        NextTick => {
            vm.next_tick(arg(0), args.get(1..).map(|r| r.to_vec()).unwrap_or_default());
            Value::Undef
        }
        FunctionBind => Value::Bound(Rc::new(Bound {
            target: this,
            this: arg(0),
            args: args.get(1..).map(|r| r.to_vec()).unwrap_or_default(),
        })),
        FunctionCall => {
            let rest = args.get(1..).map(|r| r.to_vec()).unwrap_or_default();
            vm.call(&this, arg(0), rest)?
        }
        FunctionApply => {
            let list = match arg(1) {
                Value::Undef | Value::Null => vec![],
                v => natives::iterate(vm, &v)?,
            };
            vm.call(&this, arg(0), list)?
        }
        _ => unreachable!("not a task native: {n:?}"),
    })
}

/// What the run loop does after an instruction handled out of line.
pub(super) enum Flow {
    Next,
    Jump(usize),
    /// The top frame changed: reload it.
    Switch,
    /// The run loop's base frame finished or suspended.
    Return,
}

impl Vm {
    /// Instructions kept out of the interpreter loop so it stays small:
    /// iteration, suspension, accessors. `pc` is the next instruction.
    #[inline(never)]
    pub(super) fn exec_extended(&mut self, fi: usize, pc: usize, sb: usize, vb: usize, stop: usize) -> Result<Flow, Value> {
        let func = self.frames[fi].func.get();
        let ins = func.code[pc - 1];
        let val = |vm: &Vm, r: ValSlot| vm.values[vb + r.index()].clone();
        let leave = |vm: &mut Vm, ret: Ret, out: Value| -> Result<Flow, Value> {
            vm.deliver(ret, out)?;
            Ok(if vm.frames.len() <= stop { Flow::Return } else { Flow::Switch })
        };
        match ins {
            Instruction::DefineAccessor { object, property, getter, setter } => {
                let site = &func.property_sites[property as usize];
                let args = vec![val(self, object), Value::Str(JsStr::new(&*site.name)), val(self, getter), val(self, setter)];
                call_native(self, Native::ObjDefineAccessor, Value::Undef, args)?;
            }
            Instruction::GetIterator { value, output } => {
                let it = self.get_iterator(&val(self, value))?;
                self.values[vb + output.index()] = it;
            }
            Instruction::GetAsyncIterator { value, output, is_sync } => {
                let (it, sync) = self.get_async_iterator(&val(self, value))?;
                self.values[vb + output.index()] = it;
                self.scalars[sb + is_sync.index()] = sync as u64;
            }
            Instruction::IteratorStep { iterator, output, to } => match self.iterator_step(&val(self, iterator))? {
                Some(x) => self.values[vb + output.index()] = x,
                None => return Ok(Flow::Jump(to as usize)),
            },
            Instruction::IteratorNextResult { iterator, output } => {
                let r = self.iterator_next_result(&val(self, iterator))?;
                self.values[vb + output.index()] = r;
            }
            Instruction::UnpackIteratorResult { result, output, to } => match self.unpack_result(&val(self, result))? {
                Some(x) => self.values[vb + output.index()] = x,
                None => return Ok(Flow::Jump(to as usize)),
            },
            Instruction::IteratorClose { iterator } => self.iterator_close(&val(self, iterator))?,
            Instruction::IteratorResume { iterator, mode, value, output, done } => {
                let m = match from_bits(self.scalars[sb + mode.index()]) as u8 {
                    0 => Resume::Next,
                    1 => Resume::Throw,
                    _ => Resume::Return,
                };
                let (x, d) = self.iterator_resume(&val(self, iterator), m, val(self, value))?;
                self.values[vb + output.index()] = x;
                self.scalars[sb + done.index()] = d as u64;
            }
            Instruction::CreateGenerator => {
                let kind = if func.is_async { CoKind::AsyncGenerator } else { CoKind::Generator };
                let co = Coroutine::new(kind, func.id, self.frames[fi].closure.clone());
                self.co_frames.push((fi, co.clone()));
                let fr = self.suspend_top(pc);
                co.st.borrow_mut().status = CoStatus::Start;
                return leave(self, fr.ret, Value::Gen(co));
            }
            Instruction::StartAsync => {
                let co = Coroutine::new(CoKind::Async, func.id, self.frames[fi].closure.clone());
                {
                    let mut st = co.st.borrow_mut();
                    st.status = CoStatus::Running;
                    st.promise = Some(Promise::new());
                }
                self.co_frames.push((fi, co));
            }
            Instruction::Yield { value, .. } => {
                let x = val(self, value);
                let co = self.frame_co(fi).expect("yield outside a generator");
                let fr = self.suspend_top(pc);
                let out = if co.kind == CoKind::AsyncGenerator {
                    let req = co.st.borrow_mut().queue.pop_front();
                    if let Some((_, _, p)) = req {
                        self.resolve_promise(&p, iter_result(x, false));
                    }
                    Value::Undef
                } else {
                    x
                };
                return leave(self, fr.ret, out);
            }
            Instruction::Await { value, .. } => {
                let x = val(self, value);
                let co = self.frame_co(fi).expect("await outside an async function");
                let p = self.promise_of(x);
                let fr = self.suspend_top(pc);
                co.st.borrow_mut().awaiting = true;
                self.promise_react(&p, Reaction::Resume(co.clone()));
                // The first `await` of an async call returns its promise to the caller.
                let out = match &co.st.borrow().promise {
                    Some(p) => Value::Promise(p.clone()),
                    None => Value::Undef,
                };
                return leave(self, fr.ret, out);
            }
            _ => unreachable!("not an extended instruction"),
        }
        Ok(Flow::Next)
    }
}

/// The hidden key of a function's `prototype` (kept out of its printed properties).
pub const PROTOTYPE_KEY: &str = "\u{1}prototype";

/// A function's `prototype` object, created on first use with `constructor`
/// pointing back at the function.
pub fn func_prototype(f: &Rc<Closure>) -> Rc<Object> {
    if let Some(p) = &*f.props.borrow()
        && let Some(Value::Obj(o)) = p.get(PROTOTYPE_KEY)
    {
        return o;
    }
    let proto = Object::new(Shape::prototype_of(Value::Func(f.clone())), vec![], None);
    let mut props = f.props.borrow_mut();
    props.get_or_insert_with(|| Object::new(Shape::new(vec![]), vec![], None)).set(&Rc::from(PROTOTYPE_KEY), Value::Obj(proto.clone()));
    proto
}

impl Vm {
    /// `new f(...args)` for a run-time callee: a class, or a plain function
    /// used as a constructor (ES5 style).
    pub fn construct(&mut self, f: &Value, args: Vec<Value>) -> Result<Value, Value> {
        match f {
            Value::Class(c) => {
                let o = Value::Obj(Object::blank(c.shape.clone(), Some(c.clone())));
                let ctor = Value::Func(Rc::new(Closure { func: c.ctor, caps: Box::new([]), props: Default::default() }));
                self.call(&ctor, o.clone(), args)?;
                Ok(o)
            }
            Value::Func(cl) => {
                let proto = func_prototype(cl);
                let o = Value::Obj(Object::new(Shape::with_proto(proto), vec![], None));
                let r = self.call(f, o.clone(), args)?;
                Ok(match r {
                    Value::Undef | Value::Null | Value::Bool(_) | Value::Num(_) | Value::Str(_) | Value::BigInt(_) | Value::Symbol(_) => o,
                    r => r,
                })
            }
            Value::Bound(b) => {
                let mut all = b.args.to_vec();
                all.extend(args);
                let t = b.target.clone();
                self.construct(&t, all)
            }
            _ => Err(self.type_error(format!("{} is not a constructor", describe_short(f)))),
        }
    }

    /// `x instanceof f` for a run-time `f`.
    pub fn instance_of(&mut self, x: &Value, f: &Value) -> Result<bool, Value> {
        match f {
            Value::Class(c) => Ok(match x {
                Value::Obj(o) => o.class.as_ref().is_some_and(|k| k.is_subclass_of(c.id)),
                _ => false,
            }),
            Value::Func(cl) => {
                let Value::Obj(o) = x else { return Ok(false) };
                let target = func_prototype(cl);
                let mut p = o.proto();
                while let Some(q) = p {
                    if Rc::ptr_eq(&q, &target) {
                        return Ok(true);
                    }
                    p = q.proto();
                }
                Ok(false)
            }
            Value::Bound(b) => {
                let t = b.target.clone();
                self.instance_of(x, &t)
            }
            Value::Native(_) => Ok(false),
            _ => Err(self.type_error("Right-hand side of 'instanceof' is not callable".into())),
        }
    }
}
