//! Suspendable functions (generators and async functions) and promises.
//!
//! A generator or async function runs in an ordinary VM frame. When it
//! suspends (`yield`, `await`), its slots are moved out of the VM's slot
//! stacks into its `Coroutine`, and moved back when it resumes. Nothing else
//! changes: the instructions are the same, and typed slots stay unboxed.

use crate::runtime::Gc;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use super::{Closure, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoKind {
    Generator,
    Async,
    AsyncGenerator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoStatus {
    /// Created; the body hasn't started (generators only).
    Start,
    /// Stopped at a `yield` or `await`.
    Suspended,
    Running,
    Done,
}

/// How a suspended function is resumed: `next(v)`, `throw(v)` or `return(v)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    Next = 0,
    Throw = 1,
    Return = 2,
}

#[derive(Debug)]
pub struct Coroutine {
    pub kind: CoKind,
    pub st: RefCell<CoState>,
}

#[derive(Debug)]
pub struct CoState {
    pub status: CoStatus,
    pub func: u32,
    pub closure: Option<Gc<Closure>>,
    /// Where to continue.
    pub pc: usize,
    /// The frame's slots while suspended.
    pub scalars: Vec<u64>,
    pub values: Vec<Value>,
    /// Active `try` handlers of the frame: (catch pc, exception slot).
    pub handlers: Vec<(u32, u16)>,
    /// Async functions: the promise the call returned.
    pub promise: Option<Gc<Promise>>,
    /// Async generators: pending `next`/`throw`/`return` calls, oldest first.
    pub queue: VecDeque<(Resume, Value, Gc<Promise>)>,
    /// Async generators: the current suspension is an `await`, not a `yield`.
    pub awaiting: bool,
}

impl Coroutine {
    pub fn new(kind: CoKind, func: u32, closure: Option<Gc<Closure>>) -> Gc<Coroutine> {
        Gc::new(Coroutine {
            kind,
            st: RefCell::new(CoState {
                status: CoStatus::Start,
                func,
                closure,
                pc: 0,
                scalars: Vec::new(),
                values: Vec::new(),
                handlers: Vec::new(),
                promise: None,
                queue: VecDeque::new(),
                awaiting: false,
            }),
        })
    }
}

#[derive(Debug)]
pub enum PromiseState {
    Pending(Vec<Reaction>),
    Fulfilled(Value),
    Rejected(Value),
}

#[derive(Debug)]
pub struct Promise {
    pub state: RefCell<PromiseState>,
    /// A rejection handler was attached (for unhandled-rejection reporting).
    pub handled: Cell<bool>,
}

impl Promise {
    pub fn new() -> Gc<Promise> {
        Gc::new(Promise {
            state: RefCell::new(PromiseState::Pending(Vec::new())),
            handled: Cell::new(false),
        })
    }
    pub fn is_pending(&self) -> bool {
        matches!(*self.state.borrow(), PromiseState::Pending(_))
    }
}

/// What happens when a promise settles.
#[derive(Debug)]
pub enum Reaction {
    /// `then(onFulfilled, onRejected)`, settling `result` with the outcome.
    Then {
        on_fulfilled: Value,
        on_rejected: Value,
        result: Option<Gc<Promise>>,
    },
    /// `await`: resume the suspended function.
    Resume(Gc<Coroutine>),
    /// One input of `Promise.all` / `allSettled` / `any`.
    Combine { agg: Rc<Combinator>, index: usize },
    /// `finally(f)`: call `f`, then pass the original outcome on.
    Finally { f: Value, result: Gc<Promise> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombineKind {
    All,
    AllSettled,
    Any,
}

/// Shared state of a `Promise.all`-style combination.
#[derive(Debug)]
pub struct Combinator {
    pub kind: CombineKind,
    pub values: RefCell<Vec<Value>>,
    pub remaining: Cell<usize>,
    pub result: Gc<Promise>,
}

/// A microtask.
#[derive(Debug)]
pub enum Job {
    /// Run a reaction with a settled promise's outcome.
    React { reaction: Reaction, value: Value, rejected: bool },
    /// Resolve `promise` with a thenable: call `then` on it.
    ResolveThenable { promise: Gc<Promise>, thenable: Value, then: Value },
    /// `queueMicrotask(f)` / `process.nextTick(f, ...args)`.
    Call { f: Value, args: Vec<Value> },
}

/// A function bound to a receiver and leading arguments: `f.bind(...)`, and
/// the internal functions promises hand out (resolve/reject).
#[derive(Debug)]
pub struct Bound {
    pub target: Value,
    pub this: Value,
    pub args: Vec<Value>,
}

/// A symbol. At run time a symbol-keyed property is stored under `key`, a
/// string no program can spell (it starts with `\u{2}`).
#[derive(Debug)]
pub struct Symbol {
    pub description: Option<Rc<str>>,
    pub key: Rc<str>,
}

pub const SYMBOL_PREFIX: char = '\u{2}';

/// The property key of a well-known symbol (`Symbol.iterator` →
/// "\u{2}Symbol.iterator").
pub fn well_known(name: &str) -> Rc<str> {
    format!("{SYMBOL_PREFIX}Symbol.{name}").into()
}

pub fn is_symbol_key(k: &str) -> bool {
    k.starts_with(SYMBOL_PREFIX)
}

/// An iterator over a snapshot of values (array, string, Map and Set
/// iterators, and `for-of` over them through the generic protocol).
#[derive(Debug)]
pub struct NativeIter {
    pub kind: &'static str,
    pub items: Vec<Value>,
    pub pos: Cell<usize>,
}

impl NativeIter {
    pub fn new(kind: &'static str, items: Vec<Value>) -> Gc<NativeIter> {
        Gc::new(NativeIter { kind, items, pos: Cell::new(0) })
    }
    pub fn next(&self) -> Option<Value> {
        let i = self.pos.get();
        let v = self.items.get(i).cloned();
        if v.is_some() {
            self.pos.set(i + 1);
        }
        v
    }
    pub fn rest(&self) -> Vec<Value> {
        let i = self.pos.get();
        self.pos.set(self.items.len());
        self.items[i.min(self.items.len())..].to_vec()
    }
}

/// An accessor property of a plain object (`{ get x() {…} }`), stored in the
/// property's slot. Reading or writing the property calls `get` / `set`.
#[derive(Debug)]
pub struct Accessor {
    pub get: Value,
    pub set: Value,
}
