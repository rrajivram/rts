//! Native compilation with Cranelift.
//!
//! Stage 1: numeric functions. Stage 2: class instances, arrays, then values
//! of any type, calls back into the interpreter, and closures. A function is
//! compiled when it is neither async nor a generator and its body uses only
//! what this file lowers: arithmetic, comparisons, control flow, `Math.*`,
//! fields, array elements, properties by name, global and captured
//! variables, string literals, closures, `new`, calls, `switch`, `for-of`
//! and `throw`.
//! Calls to other compiled functions are direct (methods too, when the class
//! hierarchy shows a single target); any other call (a function value, a
//! method by name, a built-in, a virtual method, a function that isn't
//! compiled) goes through the interpreter (`h_call`), which in turn enters a
//! compiled callee directly. Everything else keeps running in the
//! interpreter, which calls compiled code directly (`Vm::call_native_code`).
//!
//! **Values.** A number is an `f64`, a boolean an `i8`, a reference (a class
//! instance or array) an `i64`: the box address, or 0 for `null` and 1 for
//! `undefined`. Any other value (`T`, `string`, `any`, unions) is two `i64`
//! words, its tag and payload: `Value`'s own layout, so it moves between
//! compiled code, fields, elements and the interpreter without conversion.
//! Conversions between these are codegen's `coerce`: boxing is free,
//! unboxing checks the tag. A field is read straight from the object's
//! inline slots (`Object::field_offset`), checking the slot's tag.
//!
//! **Closures.** A captured variable lives in a cell (`Value::Cell`'s box),
//! as in the interpreter: the declaring function holds the cell's address
//! (counted) in a register and makes a new cell where codegen does (block
//! entry, a first `Init`, each iteration for per-iteration loop variables).
//! A closure's code takes its cells (`Closure::caps`) as a hidden first
//! argument. Reads and writes go through the cell like a global's. A read
//! of a global or captured variable is never a safe borrow (any call may
//! change it), and writing one makes a function impure.
//!
//! **Reference counts.** Compiled code maintains counts itself, inline (for a
//! value, inline when it is an object or array, else through a helper):
//! - Locals own a reference. Parameters (and `this`) are borrowed from the
//!   caller, unless the function assigns to them.
//! - An expression's result is either *owned* (a call's result, `new`) or
//!   *borrowed* (a variable or field read). A borrowed result used at once
//!   (a field read's base, a comparison, a truth test) costs no count update;
//!   one that must outlive other code (stored, returned, passed while later
//!   arguments run code) takes a reference first. A global variable counts
//!   as other code's: any call may reassign it.
//! - Returning a local moves its reference to the caller.
//! - A *pure* function (one that writes no field or global and calls only
//!   pure functions) can't free anything reachable from its arguments while
//!   it runs. So its variables that only ever hold such values
//!   (`x = x.next`) take no counts at all, and neither do the arguments
//!   passed to it.
//!
//! These are the count-elision rules; `--profile` builds count every update
//! compiled code still makes (`reference-count updates`).
//!
//! **Errors.** Compiled code never catches. A type error (a field read on
//! `null`, …), stack overflow or exception from code it called sets
//! `JitCtx::status`, releases the function's locals and returns; every
//! compiled caller returns in turn, and the interpreter throws at the call
//! site. Temporaries being evaluated at that moment are released too.
//!
//! Number semantics are the interpreter's: `%`, `**`, ToInt32 and the `Math`
//! functions call the same Rust code.
//!
//! **Layout.** This file: the context compiled code shares with the VM,
//! value kinds (`JK`, `JV`) and `JitFn`. `elig`: which functions compile,
//! why others don't, and walks over the HIR. `lower`: lowering to
//! Cranelift IR and `compile`. `helpers`: the runtime functions compiled
//! code calls (declared once, in `lower`'s `helpers!` table) and its
//! scope arenas. `rcsites`: profile builds' count-update attribution.
//!
//! Debugging: `RTS_JIT_DEBUG=1` lists what compiled, `RTS_JIT_WHY=<fn>|*`
//! says why a function didn't, `RTS_JIT_SKIP=<fn>,…` keeps functions in
//! the interpreter (to bisect a miscompile), `RTS_JIT_HIR=<fn>` prints a
//! function's HIR.

use std::cell::{RefCell, UnsafeCell};
use std::ptr::NonNull;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Signature, Value as CV};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClFuncId, Linkage, Module};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{LogicalOp, Name};
use crate::bytecode::Program;
use crate::hir::*;
use crate::runtime::heap::{self, Gc, Header};
use crate::runtime::natives::{self, Native};
use crate::runtime::arena::Arena;
use crate::runtime::{ClassRt, Closure, Object, Value};
use crate::types::{Kind, Type};

mod elig;
mod helpers;
mod lower;
#[cfg(feature = "profile")]
mod rcsites;

use elig::*;
use helpers::*;
use lower::*;
#[cfg(feature = "profile")]
use rcsites::*;

pub use helpers::no_arenas;
#[cfg(feature = "gc")]
pub use helpers::trace_roots;
pub use lower::compile;
pub(crate) use lower::ready;
#[cfg(feature = "profile")]
pub use rcsites::rc_sites_report;

/// Recursion limit of compiled code (as the interpreter's).
const MAX_DEPTH: u32 = 10_000;

/// State shared by compiled code and the interpreter.
#[repr(C)]
pub struct JitCtx {
    pub depth: u32,
    /// Non-zero: unwinding to the interpreter (`STATUS_*`).
    pub status: u32,
    /// The tag word of a value result (the payload is the return value).
    pub ret_tag: u64,
    /// The interpreter, for helpers that call back into it.
    pub vm: *mut crate::vm::Vm,
    /// While compiled code calls into the interpreter: its depth, so code
    /// compiled further in counts from there.
    pub base_depth: u32,
    /// The interpreter's global variables (`Vm::global_scalars`, `global_values`).
    pub global_scalars: *mut u64,
    pub global_values: *mut Value,
}

pub const STATUS_OVERFLOW: u32 = 1;
pub const STATUS_TYPE_ERROR: u32 = 2;
/// `throw new C("…")`: the interpreter constructs the error (`take_throw`).
pub const STATUS_THROW: u32 = 3;
/// An exception thrown by code a helper ran (`take_exception`).
pub const STATUS_EXCEPTION: u32 = 4;

struct CtxCell(UnsafeCell<JitCtx>);
// SAFETY: the VM, and so all compiled code, runs on one thread.
unsafe impl Sync for CtxCell {}
static CTX: CtxCell = CtxCell(UnsafeCell::new(JitCtx {
    depth: 0,
    status: 0,
    ret_tag: 0,
    vm: std::ptr::null_mut(),
    base_depth: 0,
    global_scalars: std::ptr::null_mut(),
    global_values: std::ptr::null_mut(),
}));

pub fn ctx() -> *mut JitCtx {
    CTX.0.get()
}

thread_local! {
    /// The message of the type error compiled code is unwinding with.
    static ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

thread_local! {
    /// `throw` sites of compiled code: the class and its string arguments.
    static THROW_SITES: RefCell<Vec<(u32, Vec<std::rc::Rc<str>>)>> = const { RefCell::new(Vec::new()) };
    static THROWING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The error compiled code is throwing: its class and constructor arguments.
pub fn take_throw() -> (u32, Vec<std::rc::Rc<str>>) {
    let i = THROWING.with(|t| t.get());
    THROW_SITES.with(|s| s.borrow()[i as usize].clone())
}

extern "C" fn h_throw_site(i: u32) {
    THROWING.with(|t| t.set(i));
    unsafe { (*ctx()).status = STATUS_THROW };
}

thread_local! {
    /// The exception compiled code is unwinding with (`STATUS_EXCEPTION`).
    static EXCEPTION: RefCell<Value> = const { RefCell::new(Value::Undef) };
}

pub fn take_exception() -> Value {
    EXCEPTION.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

/// Start unwinding with exception `e`.
fn raise(e: Value) {
    EXCEPTION.with(|x| *x.borrow_mut() = e);
    unsafe { (*ctx()).status = STATUS_EXCEPTION };
}

/// Take the pending error message.
pub fn take_error() -> String {
    ERROR.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

/// Entry point of a compiled function: `(ctx, args) -> result`, every
/// argument and the result as raw bits (`JK`).
pub type Entry = unsafe extern "C" fn(*mut JitCtx, *const u64) -> u64;

/// How compiled code holds a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JK {
    Num,
    Bool,
    /// A class instance or array, `null` (0) or `undefined` (1).
    Ref,
    /// Any value, as two words: its tag and its payload (`Value`'s layout).
    Val,
}

impl JK {
    pub fn kind(self) -> Kind {
        match self {
            JK::Num => Kind::Num,
            JK::Bool => Kind::Bool,
            JK::Ref | JK::Val => Kind::Val,
        }
    }
    /// Held in two words.
    fn wide(self) -> bool {
        self == JK::Val
    }
    /// Holds references that need counting.
    fn counted(self) -> bool {
        matches!(self, JK::Ref | JK::Val)
    }
}

/// What a value is, in more detail than `JK`: which kind of reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JV {
    Num,
    Bool,
    /// A class instance (or null/undefined).
    Obj,
    /// An array (or null/undefined).
    Arr,
    /// Only ever null or undefined.
    Nil,
    /// Any value.
    Val,
}

impl JV {
    pub fn jk(self) -> JK {
        match self {
            JV::Num => JK::Num,
            JV::Bool => JK::Bool,
            JV::Val => JK::Val,
            _ => JK::Ref,
        }
    }
    pub fn kind(self) -> Kind {
        self.jk().kind()
    }
}

/// A compiled function, as the interpreter calls it.
#[derive(Debug)]
pub struct JitFn {
    /// The function's id.
    pub fid: u32,
    /// Its entry point, once compiled (see `lower::ready`).
    pub entry: std::cell::Cell<Option<Entry>>,
    /// Calls before it was compiled, and how many it waits for.
    pub calls: std::cell::Cell<u32>,
    pub warm: u32,
    /// Takes its closure's cells (`Closure::caps`) as its first argument.
    pub env: bool,
    /// Takes the arena for its result next (0: the heap).
    pub rarena: bool,
    /// Takes `this` (as this) as its first argument (after the cells).
    pub this: Option<JV>,
    pub params: Vec<JV>,
    pub ret: JV,
}

impl JitFn {
    /// Write the words before the arguments at `argv` (the closure's cells,
    /// the result arena, `this`, as this function takes them): the index
    /// of the first argument, or None if compiled code can't take them.
    ///
    /// # Safety
    /// `argv` must have room for the words written (two per argument).
    #[inline(always)]
    pub(crate) unsafe fn write_prefix(&self, argv: *mut u64, closure: Option<&Gc<Closure>>, arena: u64, this: &Value) -> Option<usize> {
        let mut i = 0;
        if self.env {
            // Borrowed: the caller holds the closure.
            unsafe { argv.write(closure?.caps.as_ptr() as u64) };
            i = 1;
        }
        if self.rarena {
            unsafe { argv.add(i).write(arena) };
            i += 1;
        }
        match self.this {
            Some(k) => unsafe { Self::write_arg(argv, i, k, this) },
            None => Some(i),
        }
    }

    /// Write `v` as a `k` at `argv[i]` (a value takes two words, borrowed):
    /// the next index, or None if compiled code can't take it.
    ///
    /// # Safety
    /// `argv` must have room for the words written at `i`.
    #[inline(always)]
    pub(crate) unsafe fn write_arg(argv: *mut u64, i: usize, k: JV, v: &Value) -> Option<usize> {
        if k == JV::Val {
            let [t, p] = Self::val_words(v);
            unsafe {
                argv.add(i).write(t);
                argv.add(i + 1).write(p);
            }
            Some(i + 2)
        } else {
            unsafe { argv.add(i).write(Self::arg_bits(k, v)?) };
            Some(i + 1)
        }
    }

    /// A value as an argument's bits, if compiled code can take it.
    #[inline]
    pub fn arg_bits(k: JV, v: &Value) -> Option<u64> {
        match (k, v) {
            (JV::Obj, Value::Obj(o)) if o.class.is_some() => Some(o.header_ptr().as_ptr() as u64),
            (JV::Arr, Value::Arr(a)) => Some(a.header_ptr().as_ptr() as u64),
            (JV::Obj | JV::Arr | JV::Nil, Value::Null) => Some(0),
            (JV::Obj | JV::Arr | JV::Nil, Value::Undef) => Some(1),
            (JV::Num, Value::Num(x)) => Some(x.to_bits()),
            (JV::Bool, Value::Bool(b)) => Some(*b as u64),
            _ => None,
        }
    }
    /// A value's two words (tag, payload), not counted.
    #[inline]
    pub fn val_words(v: &Value) -> [u64; 2] {
        // Only the payload bytes a variant defines are read (a boolean's
        // upper bytes are padding): compiled code masks them the same way.
        let p = match v {
            Value::Undef | Value::Null => 0,
            Value::Bool(b) => *b as u64,
            Value::Native(n) => *n as u64,
            // SAFETY: `repr(C, u8)` puts every payload at offset 8; the
            // others are a full word (an `f64` or a pointer).
            _ => unsafe { *(v as *const Value as *const u64).add(1) },
        };
        [v.tag() as u64, p]
    }
    /// A result's bits as a value (taking over a reference's count).
    #[inline]
    pub fn ret_value(&self, r: u64) -> Value {
        match self.ret {
            JV::Num => Value::Num(f64::from_bits(r)),
            JV::Bool => Value::Bool(r != 0),
            JV::Val => std::mem::ManuallyDrop::into_inner(peek_value(unsafe { (*ctx()).ret_tag }, r)),
            k => ref_value(r, k),
        }
    }
}

/// A reference's bits as a value, taking over its count.
fn ref_value(r: u64, k: JV) -> Value {
    match r {
        0 => Value::Null,
        1 => Value::Undef,
        p => {
            let h = unsafe { NonNull::new_unchecked(p as *mut Header) };
            match k {
                JV::Arr => Value::Arr(unsafe { Gc::from_header_owned(h) }),
                _ => Value::Obj(unsafe { Gc::from_header_owned(h) }),
            }
        }
    }
}

/// A reference's bits as a value, without a count (never dropped).
fn peek_ref(r: u64, k: JV) -> std::mem::ManuallyDrop<Value> {
    std::mem::ManuallyDrop::new(ref_value(r, k))
}

static mut ENABLED: bool = true;

/// `--no-jit`.
/// Whether native code is on (`--no-jit` turns it off).
pub fn enabled() -> bool {
    unsafe { ENABLED }
}

pub fn disable() {
    unsafe { ENABLED = false };
}
