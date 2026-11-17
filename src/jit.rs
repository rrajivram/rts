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
use crate::runtime::{ClassRt, Closure, Object, Value};
use crate::types::{Kind, Type};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub entry: Entry,
    /// Takes its closure's cells (`Closure::caps`) as its first argument.
    pub env: bool,
    /// Takes `this` (as this) as its first argument (after the cells).
    pub this: Option<JV>,
    pub params: Vec<JV>,
    pub ret: JV,
}

impl JitFn {
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
pub fn disable() {
    unsafe { ENABLED = false };
}

// ───────────── helpers called by compiled code ─────────────

extern "C" fn h_mod(a: f64, b: f64) -> f64 {
    natives::js_mod(a, b)
}
extern "C" fn h_pow(a: f64, b: f64) -> f64 {
    natives::js_pow(a, b)
}
extern "C" fn h_int32(x: f64) -> i32 {
    natives::to_int32(x)
}

const MATH1: &[Native] = &[
    Native::MathRound, Native::MathSign, Native::MathSin, Native::MathCos, Native::MathTan, Native::MathAsin,
    Native::MathAcos, Native::MathAtan, Native::MathExp, Native::MathLog, Native::MathLog2, Native::MathLog10,
    Native::MathCbrt, Native::MathFround, Native::MathClz32,
];
const MATH2: &[Native] = &[Native::MathAtan2, Native::MathPow, Native::MathMin, Native::MathMax, Native::MathHypot, Native::MathImul];

extern "C" fn h_math1(i: u32, x: f64) -> f64 {
    natives::math1(MATH1[i as usize], x)
}
extern "C" fn h_math2(i: u32, a: f64, b: f64) -> f64 {
    natives::math2(MATH2[i as usize], a, b)
}

/// `new C`: a blank instance, owned by the caller.
///
/// Also the safepoint of compiled code: the cycle collector may run here.
/// That is sound because compiled code holds no uncounted pointer across a
/// call (a borrowed reference is a variable's or a caller's, both counted).
/// The tracing GC can't run here: it would need compiled frames' roots.
extern "C" fn h_new(class: *const ClassRt) -> u64 {
    #[cfg(not(feature = "gc"))]
    if crate::runtime::cycles::wanted() {
        crate::runtime::cycles::collect();
    }
    let c = unsafe {
        std::rc::Rc::increment_strong_count(class);
        std::rc::Rc::from_raw(class)
    };
    Gc::into_header(Object::blank(c.shape.clone(), Some(c))).as_ptr() as u64
}

/// The slow half of a decrement (the count may reach zero, or the box may
/// be a cycle candidate): the full decrement.
extern "C" fn h_dec(p: u64) {
    heap::dec_header(unsafe { NonNull::new_unchecked(p as *mut Header) });
}

/// A value from its two words (tag word, payload), not owned.
fn peek_value(tag: u64, payload: u64) -> std::mem::ManuallyDrop<Value> {
    std::mem::ManuallyDrop::new(unsafe { std::mem::transmute::<[u64; 2], Value>([tag & 0xff, payload]) })
}

/// Drop a value overwritten in a field.
extern "C" fn h_drop_val(tag: u64, payload: u64) {
    drop(std::mem::ManuallyDrop::into_inner(peek_value(tag, payload)));
}

// ── arrays (the interpreter's semantics: see `ReadElement*`, `WriteElement*`) ──

/// What an element read wants (`h_arr_get`'s `want`).
const W_NUM: u32 = 0;
const W_BOOL: u32 = 1;
const W_OBJ: u32 = 2;
const W_ARR: u32 = 3;
const W_NIL: u32 = 4;
/// Any value: the payload, with the tag in `JitCtx::ret_tag` (borrowed).
const W_VAL: u32 = 5;

fn type_error(msg: String) {
    ERROR.with(|e| *e.borrow_mut() = msg);
    unsafe { (*ctx()).status = STATUS_TYPE_ERROR };
}

/// An array's bits as a borrowed value (null and undefined too).
fn peek_arr(a: u64) -> std::mem::ManuallyDrop<Value> {
    peek_ref(a, JV::Arr)
}

fn valid_index(i: f64) -> Option<usize> {
    (i >= 0.0 && i.fract() == 0.0).then_some(i as usize)
}

/// `a[i]`, as `want`: the bits of a number or boolean, or a borrowed reference.
extern "C" fn h_arr_get(a: u64, i: f64, want: u32) -> u64 {
    use crate::runtime::ArrStore;
    let base = peek_arr(a);
    let Value::Arr(arr) = &*base else {
        let d = crate::vm::describe(&base);
        type_error(if want == W_NUM {
            format!("cannot index {d}")
        } else {
            format!("Cannot read properties of {} (reading '{}')", base.to_display(), crate::runtime::num_to_string(i))
        });
        return 0;
    };
    let st = arr.store.borrow();
    if want == W_NUM {
        return match &*st {
            ArrStore::Num(v) => valid_index(i).and_then(|i| v.get(i)).copied().unwrap_or(f64::NAN).to_bits(),
            ArrStore::Val(v) => match valid_index(i).and_then(|i| v.get(i)) {
                Some(Value::Num(x)) => x.to_bits(),
                None | Some(Value::Undef) => f64::NAN.to_bits(),
                Some(other) => {
                    type_error(format!("expected a number element, got {}", crate::vm::describe(other)));
                    0
                }
            },
        };
    }
    static UNDEF: UndefCell = UndefCell(Value::Undef);
    if want == W_VAL {
        let x = match &*st {
            ArrStore::Num(v) => valid_index(i).and_then(|i| v.get(i)).map_or(Value::Undef, |n| Value::Num(*n)),
            ArrStore::Val(v) => match valid_index(i).and_then(|i| v.get(i)) {
                // Borrowed: the array keeps it alive.
                Some(x) => return give_borrowed(x),
                None => Value::Undef,
            },
        };
        return give_borrowed(&x);
    }
    let num;
    let x = match &*st {
        ArrStore::Num(v) => match valid_index(i).and_then(|i| v.get(i)) {
            Some(n) => {
                num = Value::Num(*n);
                &num
            }
            None => &UNDEF.0,
        },
        ArrStore::Val(v) => valid_index(i).and_then(|i| v.get(i)).unwrap_or(&UNDEF.0),
    };
    match (want, x) {
        (W_BOOL, Value::Bool(b)) => *b as u64,
        (W_BOOL, other) => {
            type_error(format!("expected a boolean, got {}", crate::vm::describe(other)));
            0
        }
        (_, Value::Null) => 0,
        (_, Value::Undef) => 1,
        (W_OBJ, Value::Obj(o)) => o.header_ptr().as_ptr() as u64,
        (W_ARR, Value::Arr(o)) => o.header_ptr().as_ptr() as u64,
        (_, other) => {
            let what = if want == W_ARR { "an array" } else { "an object" };
            type_error(format!("expected {what} element, got {}", crate::vm::describe(other)));
            0
        }
    }
}

struct UndefCell(Value);
// SAFETY: `undefined` holds no pointer; it is only read.
unsafe impl Sync for UndefCell {}

/// `a[i] = v`, taking over `v`'s count.
extern "C" fn h_arr_set(a: u64, i: f64, tag: u64, payload: u64, num: u32) {
    let v = std::mem::ManuallyDrop::into_inner(peek_value(tag, payload));
    let base = peek_arr(a);
    match &*base {
        Value::Arr(arr) => {
            if let Some(i) = valid_index(i) {
                arr.set(i, v)
            }
        }
        other => {
            let d = crate::vm::describe(other);
            type_error(if num != 0 { format!("cannot index {d}") } else { format!("Cannot set properties of {d}") });
            drop(v);
        }
    }
}

/// `a.length`.
extern "C" fn h_arr_len(a: u64) -> f64 {
    match &*peek_arr(a) {
        Value::Arr(arr) => arr.len() as f64,
        other => {
            type_error(format!("Cannot read properties of {} (reading 'length')", other.to_display()));
            0.0
        }
    }
}

/// `a.push(v)`, taking over `v`'s count: the new length.
extern "C" fn h_arr_push(a: u64, tag: u64, payload: u64) -> f64 {
    let v = std::mem::ManuallyDrop::into_inner(peek_value(tag, payload));
    match &*peek_arr(a) {
        Value::Arr(arr) => {
            arr.push(v);
            arr.len() as f64
        }
        other => {
            type_error(format!("Cannot read properties of {} (reading 'push')", other.to_display()));
            drop(v);
            0.0
        }
    }
}

/// `[]`: a new array (number storage or not), owned by the caller.
extern "C" fn h_arr_new(numbers: u32) -> u64 {
    #[cfg(not(feature = "gc"))]
    if crate::runtime::cycles::wanted() {
        crate::runtime::cycles::collect();
    }
    let a = if numbers != 0 { crate::runtime::Array::new_num(Vec::new()) } else { crate::runtime::Array::new_val(Vec::new()) };
    Gc::into_header(a).as_ptr() as u64
}

/// Return a value's words from a helper: the payload, and its tag in
/// `JitCtx::ret_tag`. The count is not taken.
fn give_borrowed(v: &Value) -> u64 {
    let [t, p] = JitFn::val_words(v);
    unsafe { (*ctx()).ret_tag = t };
    p
}

/// Return a value from a helper, handing its count to compiled code.
fn give(v: Value) -> u64 {
    let p = give_borrowed(&v);
    std::mem::forget(v);
    p
}

/// Return a helper's result, or start unwinding with its exception
/// (returning `undefined`).
fn give_result(r: Result<Value, Value>) -> u64 {
    match r {
        Ok(v) => give(v),
        Err(e) => {
            raise(e);
            give(Value::Undef)
        }
    }
}

/// Take a reference to a value given as words (one of a kind compiled
/// code doesn't count inline).
/// A new cell holding undefined (its box address, counted).
extern "C" fn h_new_cell() -> u64 {
    Gc::into_header(Gc::new(RefCell::new(Value::Undef))).as_ptr() as u64
}

/// A closure of function `func` capturing the `n` cells at `cells`
/// (borrowed box addresses): its box address, counted.
extern "C" fn h_closure(func: u32, cells: *const u64, n: u32) -> u64 {
    let caps: Box<[Gc<RefCell<Value>>]> = (0..n as usize)
        .map(|i| {
            let h = unsafe { NonNull::new_unchecked(*cells.add(i) as *mut Header) };
            let c = std::mem::ManuallyDrop::new(unsafe { Gc::<RefCell<Value>>::from_header_owned(h) });
            (*c).clone()
        })
        .collect();
    Gc::into_header(Gc::new(Closure { func, caps, props: Default::default() })).as_ptr() as u64
}

/// `throw v`, taking over `v`'s count.
extern "C" fn h_throw_value(tag: u64, payload: u64) {
    raise(std::mem::ManuallyDrop::into_inner(peek_value(tag, payload)));
}

/// The array a `for-of` walks (counted): an array itself, or a new array
/// of another iterable's values. Errors (not iterable) set the status.
extern "C" fn h_iter_array(tag: u64, payload: u64) -> u64 {
    let v = peek_value(tag, payload);
    let r = match &*v {
        Value::Arr(a) => Ok(Value::Arr(a.clone())),
        _ => nested(|vm| vm.jit_iteration_array(&v)),
    };
    match r {
        Ok(Value::Arr(ref a)) => Gc::into_header(a.clone()).as_ptr() as u64,
        Ok(_) => unreachable!("an iteration array"),
        Err(e) => {
            raise(e);
            1
        }
    }
}

extern "C" fn h_inc_val(tag: u64, payload: u64) {
    std::mem::forget((*peek_value(tag, payload)).clone());
}

thread_local! {
    /// Values compiled code uses as constants (string literals), kept alive
    /// for good, and its property sites. Roots of the tracing GC.
    static CONSTS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    static SITES: RefCell<Vec<&'static crate::bytecode::PropertySite>> = const { RefCell::new(Vec::new()) };
}

/// Trace the values compiled code holds for good (tracing-GC build).
#[cfg(feature = "gc")]
pub fn trace_roots(t: &mut heap::Tracer) {
    use heap::Trace;
    CONSTS.with(|c| c.borrow().iter().for_each(|v| v.trace(t)));
    SITES.with(|s| s.borrow().iter().for_each(|x| x.key.trace(t)));
}

/// A constant's words, the constant kept alive for good.
fn constant(v: Value) -> [u64; 2] {
    let w = JitFn::val_words(&v);
    CONSTS.with(|c| c.borrow_mut().push(v));
    w
}

/// A property site of compiled code (its own lookup cache).
fn site(name: &str) -> &'static crate::bytecode::PropertySite {
    let s: &'static _ = Box::leak(Box::new(crate::bytecode::PropertySite::new(name)));
    SITES.with(|x| x.borrow_mut().push(s));
    s
}

/// The interpreter, while compiled code runs.
fn vm() -> &'static mut crate::vm::Vm {
    // SAFETY: set by `Vm::call_native_code` before entering compiled code,
    // which is only entered from there; the interpreter isn't otherwise
    // touched while compiled code runs.
    unsafe { &mut *(*ctx()).vm }
}

/// Run `f` (a call into the interpreter) with the depth of compiled code
/// counting toward the interpreter's, and restore it after.
fn nested<T>(f: impl FnOnce(&mut crate::vm::Vm) -> T) -> T {
    let c = ctx();
    let (depth, base) = unsafe { ((*c).depth, (*c).base_depth) };
    unsafe { (*c).base_depth = depth };
    let r = f(vm());
    unsafe {
        (*c).depth = depth;
        (*c).base_depth = base;
    }
    r
}

/// What `h_call` calls.
const CALL_FUNC: u32 = 0;
const CALL_VSLOT: u32 = 1;
const CALL_NAMED: u32 = 2;
const CALL_VALUE: u32 = 3;
const CALL_NATIVE: u32 = 4;
const CALL_CONSTRUCT: u32 = 5;

/// A call into the interpreter: `kind` says what `a` (and `b`) are; `this`
/// and the `argc` arguments at `argv` are borrowed values. The result is
/// owned (see `give`).
extern "C" fn h_call(kind: u32, a: u64, b: u64, this_t: u64, this_p: u64, argv: *const Value, argc: u32) -> u64 {
    if unsafe { (*ctx()).depth } >= MAX_DEPTH {
        unsafe { (*ctx()).status = STATUS_OVERFLOW };
        return give(Value::Undef);
    }
    // No arguments: compiled code passes a null `argv`.
    let args: &[Value] = if argc == 0 { &[] } else { unsafe { std::slice::from_raw_parts(argv, argc as usize) } };
    let this = (*peek_value(this_t, this_p)).clone();
    let r = nested(|vm| match kind {
        CALL_FUNC => vm.jit_call_func(a as u32, this, args),
        CALL_VSLOT => vm.jit_call_vslot(a as u32, this, args),
        CALL_NAMED => vm.jit_call_named(unsafe { &*(a as *const crate::bytecode::PropertySite) }, this, args),
        CALL_VALUE => vm.jit_call_value(&peek_value(a, b), this, args),
        CALL_NATIVE => vm.jit_call_native(unsafe { std::mem::transmute::<u16, Native>(a as u16) }, this, args),
        _ => vm.jit_construct(a as u32, args),
    });
    give_result(r)
}

/// `obj.name` (the site's), as an owned value.
extern "C" fn h_get_prop(site: *const crate::bytecode::PropertySite, t: u64, p: u64) -> u64 {
    let r = nested(|vm| vm.jit_get_prop(unsafe { &*site }, &peek_value(t, p)));
    give_result(r)
}

/// `obj.name = v`, taking over `v`'s count.
extern "C" fn h_set_prop(site: *const crate::bytecode::PropertySite, t: u64, p: u64, vt: u64, vp: u64) {
    let v = std::mem::ManuallyDrop::into_inner(peek_value(vt, vp));
    if let Err(e) = nested(|vm| vm.jit_set_prop(unsafe { &*site }, &peek_value(t, p), v)) {
        raise(e);
    }
}

/// Operations on values (`h_op`).
const OP_STRICT_EQ: u32 = 0;
const OP_LOOSE_EQ: u32 = 1;
const OP_TRUTHY: u32 = 2;
const OP_CONCAT: u32 = 3;
const OP_TO_STR: u32 = 4;
const OP_TYPEOF: u32 = 5;
/// `s.length`: a number's bits.
const OP_STR_LEN: u32 = 6;
/// `OP_STR_CMP + CmpOp as u32`: string comparisons.
const OP_STR_CMP: u32 = 8;

/// An operation on one or two borrowed values: a boolean (0/1), or an owned
/// value (see `give`).
extern "C" fn h_op(op: u32, at: u64, ap: u64, bt: u64, bp: u64) -> u64 {
    let (a, b) = (peek_value(at, ap), peek_value(bt, bp));
    match op {
        OP_STRICT_EQ => a.strict_eq(&b) as u64,
        OP_LOOSE_EQ => a.loose_eq(&b) as u64,
        OP_TRUTHY => a.truthy() as u64,
        // As the interpreter's `Concat`.
        OP_CONCAT => give(match (&*a, &*b) {
            (Value::Str(x), Value::Str(y)) if y.s.is_empty() => Value::Str(x.clone()),
            (Value::Str(x), Value::Str(y)) if x.s.is_empty() => Value::Str(y.clone()),
            (Value::Str(x), Value::Str(y)) => {
                let mut st = String::with_capacity(x.s.len() + y.s.len());
                st.push_str(&x.s);
                st.push_str(&y.s);
                Value::str(st)
            }
            (x, y) => Value::str(format!("{}{}", x.to_js_string().s, y.to_js_string().s)),
        }),
        OP_TO_STR => {
            let r = nested(|vm| vm.jit_to_string(&a));
            give_result(r)
        }
        OP_TYPEOF => give(Value::str(a.type_of())),
        OP_STR_LEN => match &*a {
            Value::Str(st) => (st.len16() as f64).to_bits(),
            other => {
                type_error(format!("Cannot read properties of {} (reading 'length')", other.to_display()));
                0
            }
        },
        _ => {
            let cmp = [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne][(op - OP_STR_CMP) as usize];
            (match (&*a, &*b) {
                (Value::Str(x), Value::Str(y)) => crate::vm::str_cmp(cmp, &x.s, &y.s),
                (x, y) => crate::vm::str_cmp(cmp, &x.to_js_string().s, &y.to_js_string().s),
            }) as u64
        }
    }
}

const ERR_READ: u32 = 0;
const ERR_WRITE: u32 = 1;
const ERR_METHOD: u32 = 2;
const ERR_FIELD_NUM: u32 = 3;
const ERR_FIELD_BOOL: u32 = 4;
const ERR_FIELD_REF: u32 = 5;
/// Unboxing a value (codegen's `coerce`).
const ERR_UNBOX_NUM: u32 = 6;
const ERR_UNBOX_BOOL: u32 = 7;
const ERR_UNBOX_REF: u32 = 8;

/// Start unwinding with a type error.
extern "C" fn h_throw(code: u32, a: u64, b: u64) {
    let nullish = |p: u64| if p == 0 { "null" } else { "undefined" };
    let msg = match code {
        ERR_READ => format!("Cannot read properties of {}", nullish(a)),
        ERR_WRITE => format!("Cannot set properties of {}", nullish(a)),
        ERR_METHOD => format!("cannot call a method on {}", nullish(a)),
        ERR_UNBOX_NUM | ERR_UNBOX_BOOL | ERR_UNBOX_REF => {
            let v = peek_value(a, b);
            let what = match code {
                ERR_UNBOX_NUM => "a number",
                ERR_UNBOX_BOOL => "a boolean",
                _ => "an object",
            };
            format!("expected {what}, got {}", crate::vm::describe(&v))
        }
        _ => {
            let v = peek_value(a, b);
            let what = match code {
                ERR_FIELD_NUM => "a number",
                ERR_FIELD_BOOL => "a boolean",
                _ => "an object",
            };
            format!("expected {what} field, got {}", crate::vm::describe(&v))
        }
    };
    ERROR.with(|e| *e.borrow_mut() = msg);
    unsafe { (*ctx()).status = STATUS_TYPE_ERROR };
}

// ───────────── eligibility ─────────────

/// Which kind of reference a value of type `t` is, if compiled code can
/// hold it (a class instance or an array, never both, or null/undefined).
fn ref_class(t: &Type) -> Option<JV> {
    match t {
        Type::Class(..) => Some(JV::Obj),
        Type::Array(_) => Some(JV::Arr),
        // Not `void`: a `() => void` function value may return anything.
        Type::Null | Type::Undefined => Some(JV::Nil),
        Type::Union(ts) => ts.iter().try_fold(JV::Nil, |acc, t| match (acc, ref_class(t)?) {
            (a, JV::Nil) => Some(a),
            (JV::Nil, b) => Some(b),
            (a, b) if a == b => Some(a),
            _ => None,
        }),
        _ => None,
    }
}

/// How compiled code would hold a value of type `t`, if it can.
fn jv(t: &Type) -> Option<JV> {
    match t.kind() {
        Kind::Num => Some(JV::Num),
        Kind::Bool => Some(JV::Bool),
        Kind::Big => None,
        Kind::Val => Some(ref_class(t).unwrap_or(JV::Val)),
    }
}

/// Can compiled code convert a `from` to a `to` (as codegen's `coerce`
/// does: boxing, or unboxing with a check)?
fn coercible(from: JK, to: JK) -> bool {
    from == to || from == JK::Val || to == JK::Val
}

fn jk(t: &Type) -> Option<JK> {
    jv(t).map(JV::jk)
}

/// A function's result, in detail.
fn ret_jv(hf: &HFunc) -> Option<JV> {
    match hf.ret_kind {
        None => Some(JV::Nil),
        Some(Kind::Num) => Some(JV::Num),
        Some(Kind::Bool) => Some(JV::Bool),
        Some(Kind::Val) => Some(hf.ret_ty.as_ref().and_then(ref_class).unwrap_or(JV::Val)),
        Some(Kind::Big) => None,
    }
}

/// A function's result, as compiled code returns it (`void` is `undefined`).
fn ret_jk(hf: &HFunc) -> Option<JK> {
    match hf.ret_kind {
        None => Some(JK::Ref),
        k => ret_jv(hf).filter(|_| k.is_some()).map(JV::jk),
    }
}

/// Which functions can be compiled (a fixpoint: calls must go to compiled functions).
struct Elig<'a> {
    hir: &'a HProgram,
    set: FxHashSet<FuncId>,
    pure: FxHashSet<FuncId>,
    /// Each class's subclasses (itself included).
    subclasses: Vec<Vec<u32>>,
}

impl Elig<'_> {
    fn local(&self, f: FuncId, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(f) && !vi.captured && jk(&vi.ty).is_some()
    }

    /// `f`'s own variable that a closure captured: it lives in a cell.
    fn cell(&self, f: FuncId, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(f) && vi.captured && jk(&vi.ty).is_some()
    }

    /// An enclosing function's variable `f` uses (through its closure's cells).
    fn free(&self, f: FuncId, v: VarId) -> bool {
        self.hir.funcs[f as usize].free.contains(&v) && jk(&self.hir.vars[v as usize].ty).is_some()
    }

    /// A variable compiled code can read and write, wherever it lives.
    fn var_ok(&self, f: FuncId, v: VarId) -> bool {
        self.local(f, v) || self.cell(f, v) || self.free(f, v) || self.global(v)
    }

    /// A variable `f` declares (in a register or a cell).
    fn own_var(&self, f: FuncId, v: VarId) -> bool {
        self.local(f, v) || self.cell(f, v)
    }

    /// `f` can make a closure of `g`: each cell `g` captures is one of
    /// `f`'s or one `f` captured itself.
    fn closure_ok(&self, f: FuncId, g: FuncId) -> bool {
        let hf = &self.hir.funcs[f as usize];
        self.hir.funcs[g as usize].free.iter().all(|v| {
            let vi = &self.hir.vars[*v as usize];
            (vi.owner == Some(f) && vi.captured) || hf.free.contains(v)
        })
    }

    /// A global variable compiled code can read and write.
    fn global(&self, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner.is_none() && jk(&vi.ty).is_some()
    }

    fn vjk(&self, v: VarId) -> Option<JK> {
        jk(&self.hir.vars[v as usize].ty)
    }

    /// The function a call reaches, if known (devirtualized).
    fn target(&self, c: &HCall) -> Option<FuncId> {
        match &c.callee {
            Callee::Direct { func, .. } => Some(*func),
            Callee::Method { class, vslot } => self.devirt(*class, *vslot),
            _ => None,
        }
    }

    /// Does evaluating `e` run code that could change a variable
    /// (`assigns_only`), or free or change anything on the heap? Calls to
    /// pure functions and `Math` change nothing. Only for expressions `Elig`
    /// accepted.
    fn effects(&self, e: &HExpr, assigns_only: bool) -> bool {
        let top = match &e.kind {
            HK::Assign(..) | HK::Compound(..) | HK::Update { .. } | HK::LogicalAssign(..) => true,
            HK::Call(c) => !assigns_only && !self.pure_call(c),
            // These may run any code (a getter, `toString`, a constructor).
            HK::New { .. } | HK::Array(_) | HK::NewBuiltin(..) | HK::Unary(HUnary::ToStr, _) | HK::Binary(HBin::DynAdd, ..) => !assigns_only,
            HK::Read(p) => !assigns_only && matches!(**p, Place::Prop { .. }),
            _ => false,
        };
        top || children(e).into_iter().any(|x| self.effects(x, assigns_only))
    }

    /// A call that changes nothing: `Math`, or a pure function.
    fn pure_call(&self, c: &HCall) -> bool {
        match c.callee {
            Callee::Native(n) => math_native(n),
            _ => self.target(c).is_some_and(|g| self.pure.contains(&g)),
        }
    }

    /// Pure functions write no field and call only pure functions: while one
    /// runs, nothing reachable from its arguments can change or be freed.
    fn compute_pure(&mut self) {
        self.pure = self.set.clone();
        loop {
            let keep: FxHashSet<FuncId> = self.pure.iter().copied().filter(|&f| self.pure_stmts(&self.hir.funcs[f as usize].body)).collect();
            if keep.len() == self.pure.len() {
                break;
            }
            self.pure = keep;
        }
    }

    fn pure_stmts(&self, ss: &[HStmt]) -> bool {
        ss.iter().all(|s| {
            // A literal throw site constructs its error after unwinding.
            if let HStmt::Throw(e) = s
                && throw_site(e).is_some()
            {
                return true;
            }
            let (xs, bodies) = parts(s);
            xs.into_iter().all(|x| self.pure_expr(x)) && bodies.into_iter().all(|b| self.pure_stmts(b))
        })
    }

    fn pure_expr(&self, e: &HExpr) -> bool {
        let top = match &e.kind {
            HK::Assign(p, _) | HK::Compound(p, _, _) | HK::Update { place: p, .. } => {
                matches!(**p, Place::Var(v) if self.hir.vars[v as usize].owner.is_some() && !self.hir.vars[v as usize].captured)
            }
            HK::New { .. } | HK::Array(_) | HK::NewBuiltin(..) | HK::Unary(HUnary::ToStr, _) | HK::Binary(HBin::DynAdd, ..) => false,
            HK::Read(p) => !matches!(**p, Place::Prop { .. }),
            HK::Call(c) => self.pure_call(c),
            _ => true,
        };
        top && children(e).into_iter().all(|x| self.pure_expr(x))
    }

    /// In a pure function: the reference variables that need no counts.
    /// Each holds only values reachable from the arguments (a parameter's,
    /// another such variable's, a field of one, null): nothing can free them
    /// while the function runs.
    fn borrowed_vars(&self, f: FuncId, vars: &[VarId]) -> FxHashSet<VarId> {
        let hf = &self.hir.funcs[f as usize];
        if !self.pure.contains(&f) {
            return FxHashSet::default();
        }
        let mut set: FxHashSet<VarId> = vars.iter().copied().filter(|v| self.vjk(*v) == Some(JK::Ref) && !self.hir.vars[*v as usize].captured).collect();
        loop {
            let mut assigns: Vec<(VarId, &HExpr)> = Vec::new();
            collect_ref_assigns(&hf.body, &mut assigns);
            let bad: FxHashSet<VarId> = assigns.iter().filter(|(v, x)| set.contains(v) && !safe_borrow(x, &set)).map(|(v, _)| *v).collect();
            if bad.is_empty() {
                return set;
            }
            set.retain(|v| !bad.contains(v));
        }
    }

    /// The one function a virtual call can reach, if the class hierarchy
    /// shows only one.
    fn devirt(&self, class: u32, vslot: u32) -> Option<FuncId> {
        let mut target = None;
        for &k in &self.subclasses[class as usize] {
            let f = self.hir.classes[k as usize].vtable.get(vslot as usize)?.1;
            match target {
                None => target = Some(f),
                Some(g) if g == f => {}
                Some(_) => return None,
            }
        }
        target
    }

    fn func_ok(&self, f: FuncId) -> bool {
        let hf = &self.hir.funcs[f as usize];
        if why_wanted(&hf.name) && !why_all() {
            let vars: Vec<String> = hf
                .this_var
                .iter()
                .chain(hf.params.iter().map(|p| &p.var))
                .map(|v| {
                    let vi = &self.hir.vars[*v as usize];
                    format!("{}: {:?} (captured {}, local {})", vi.name, vi.ty, vi.captured, self.local(f, *v))
                })
                .collect();
            eprintln!(
                "rts jit: {}: ret {:?} {:?}, async {}, generator {}, free {:?}, params {vars:?}",
                hf.name, hf.ret_kind, hf.ret_ty, hf.is_async, hf.is_generator, hf.free
            );
        }
        let Some(rk) = ret_jk(hf) else { return false };
        // `RTS_JIT_HIR=name`: print the function's HIR.
        if std::env::var("RTS_JIT_HIR").is_ok_and(|n| n == *hf.name) {
            eprintln!("rts jit: {}: {:#?}", hf.name, hf.body);
        }
        // `RTS_JIT_SKIP=a,b`: leave these to the interpreter (to bisect a miscompile).
        if let Ok(skip) = std::env::var("RTS_JIT_SKIP")
            && skip.split(',').any(|n| n == &*hf.name)
        {
            return false;
        }
        let reject = |msg: String| {
            if why_wanted(&hf.name) && f != self.hir.main {
                WHY_FN.with(|w| *w.borrow_mut() = hf.name.to_string());
                why_say(msg);
            }
            false
        };
        if f == self.hir.main {
            return false;
        }
        if hf.is_async || hf.is_generator {
            return reject("async or generator".into());
        }
        if let Some(t) = hf.this_var
            && !(self.own_var(f, t) && self.vjk(t).is_some_and(JK::counted))
        {
            return reject(format!("`this` is {:?}", self.hir.vars[t as usize].ty));
        }
        for p in &hf.params {
            if p.default.is_some() || p.rest || !self.own_var(f, p.var) || self.vjk(p.var).map(JK::kind) != Some(p.arrival) {
                let vi = &self.hir.vars[p.var as usize];
                return reject(format!(
                    "parameter {} (default {}, rest {}, captured {})",
                    vi.name,
                    p.default.is_some(),
                    p.rest,
                    vi.captured
                ));
            }
        }
        // Falling off the end would return undefined, which the interpreter
        // reports as a type error for a number or boolean result.
        if !rk.counted() && !always_returns(&hf.body) {
            return reject("may fall off the end".into());
        }
        let ok = hf.body.iter().all(|s| self.stmt(f, s));
        if !ok && why_wanted(&hf.name) {
            WHY_FN.with(|w| *w.borrow_mut() = hf.name.to_string());
            self.why(f, &hf.body);
        }
        ok
    }

    /// `RTS_JIT_WHY=<function>`: print the innermost statement or expression
    /// that keeps the function from compiling.
    fn why(&self, f: FuncId, ss: &[HStmt]) {
        for s in ss {
            if self.stmt(f, s) {
                continue;
            }
            match s {
                HStmt::Block(_, vars) if !vars.iter().all(|v| self.own_var(f, *v)) => {
                    let names: Vec<_> = vars.iter().filter(|v| !self.own_var(f, **v)).map(|v| self.hir.vars[*v as usize].name.to_string()).collect();
                    why_say(format!("block variables {names:?}"))
                }
                HStmt::Block(b, _) => self.why(f, b),
                HStmt::If(c, t, e) if self.expr(f, c).is_some() => {
                    self.why(f, t);
                    self.why(f, e);
                }
                HStmt::Loop { body, fresh, test, update, .. } => {
                    if !fresh.iter().all(|v| self.own_var(f, *v)) {
                        why_say(format!("loop with per-iteration variables {fresh:?}"));
                    } else if let Some(t) = test.as_ref().filter(|t| self.expr(f, t).is_none()) {
                        self.why_expr(f, t);
                    } else if let Some(u) = update.as_ref().filter(|u| self.expr(f, u).is_none()) {
                        self.why_expr(f, u);
                    } else {
                        self.why(f, body);
                    }
                }
                HStmt::Expr(e) | HStmt::Return(Some(e)) | HStmt::Init(_, Some(e)) | HStmt::Throw(e) => self.why_expr(f, e),
                HStmt::Switch { disc, cases, .. } => match std::iter::once(disc).chain(cases.iter().filter_map(|c| c.0.as_ref())).find(|x| self.expr(f, x).is_none()) {
                    Some(x) => self.why_expr(f, x),
                    None => cases.iter().for_each(|c| self.why(f, &c.1)),
                },
                HStmt::ForOf { iter, body, is_await: false, .. } if self.expr(f, iter).is_some() => self.why(f, body),
                _ => why_say(format!("{s:?}")),
            }
            return;
        }
    }

    fn why_expr(&self, f: FuncId, e: &HExpr) {
        let subs = children(e);
        for x in subs {
            if self.expr(f, x).is_none() {
                return self.why_expr(f, x);
            }
        }
        why_say(format!("({:?}): {:?}", e.ty, e.kind));
    }

    fn stmt(&self, f: FuncId, s: &HStmt) -> bool {
        match s {
            HStmt::Expr(e) => self.expr(f, e).is_some(),
            HStmt::Init(v, e) => self.own_var(f, *v) && e.as_ref().is_none_or(|e| self.fits(f, e, self.vjk(*v).unwrap())),
            HStmt::MakeClosure(v, g) => self.own_var(f, *v) && self.vjk(*v) == Some(JK::Val) && self.closure_ok(f, *g),
            HStmt::Block(ss, vars) => vars.iter().all(|v| self.own_var(f, *v)) && ss.iter().all(|s| self.stmt(f, s)),
            HStmt::If(c, t, e) => self.cond(f, c) && t.iter().chain(e.iter()).all(|s| self.stmt(f, s)),
            HStmt::Throw(e) => throw_site(e).is_some() || self.fits(f, e, JK::Val),
            HStmt::Switch { disc, cases, .. } => {
                let dk = self.expr(f, disc);
                dk.is_some()
                    && cases.iter().all(|(t, body)| t.as_ref().is_none_or(|t| self.expr(f, t).is_some()) && body.iter().all(|s| self.stmt(f, s)))
            }
            // Over an array as it is, or over a copy of another iterable's values.
            HStmt::ForOf { elem, iter, body, is_await: false, .. } => {
                self.own_var(f, *elem) && self.fits(f, iter, JK::Val) && body.iter().all(|s| self.stmt(f, s))
            }
            // Captured per-iteration variables get a new cell each iteration.
            HStmt::Loop { test, body, update, fresh, .. } => {
                fresh.iter().all(|v| self.own_var(f, *v))
                    && test.as_ref().is_none_or(|t| self.cond(f, t))
                    && update.as_ref().is_none_or(|u| self.expr(f, u).is_some())
                    && body.iter().all(|s| self.stmt(f, s))
            }
            HStmt::Break(_) | HStmt::Continue(_) => true,
            HStmt::Labeled(_, s) => self.stmt(f, s),
            HStmt::Return(None) => ret_jk(&self.hir.funcs[f as usize]).is_some_and(JK::counted),
            HStmt::Return(Some(e)) => self.fits(f, e, ret_jk(&self.hir.funcs[f as usize]).unwrap()),
            _ => false,
        }
    }

    /// The expression's `JK`, if compiled code can evaluate it.
    fn expr(&self, f: FuncId, e: &HExpr) -> Option<JK> {
        let k = jk(&e.ty)?;
        let is = |x: &HExpr, want: JK| self.expr(f, x) == Some(want);
        let fits = |x: &HExpr, want: JK| self.fits(f, x, want);
        let ok = match &e.kind {
            HK::Num(_) => k == JK::Num,
            HK::Bool(_) => k == JK::Bool,
            HK::Null | HK::Undefined => k == JK::Ref,
            HK::Str(_) => k == JK::Val,
            HK::Closure(g) => k == JK::Val && self.closure_ok(f, *g),
            HK::Read(p) => match &**p {
                Place::ArrLen { obj, opt: false } => k == JK::Num && self.is_array(f, obj),
                Place::StrLen { obj, opt: false } => k == JK::Num && self.fits(f, obj, JK::Val),
                // Narrowing a global or captured variable is unsound (a call
                // may change it), and the interpreter doesn't check it: a
                // value narrowed to an object or null may be anything.
                Place::Var(v) if k == JK::Ref && self.vjk(*v) == Some(JK::Val) && !self.local(f, *v) => false,
                p => self.place(f, p, k),
            },
            HK::Binary(HBin::Num(_), a, b) | HK::Binary(HBin::NumCmp(_), a, b) => is(a, JK::Num) && is(b, JK::Num),
            HK::Binary(HBin::BoolEq(_), a, b) => is(a, JK::Bool) && is(b, JK::Bool),
            // Mixed kinds compare as values.
            HK::Binary(HBin::StrictEq(_) | HBin::LooseEq(_), a, b) => k == JK::Bool && self.expr(f, a).is_some() && self.expr(f, b).is_some(),
            HK::Binary(HBin::Concat, a, b) => k == JK::Val && fits(a, JK::Val) && fits(b, JK::Val),
            HK::Binary(HBin::StrCmp(_), a, b) => k == JK::Bool && fits(a, JK::Val) && fits(b, JK::Val),
            HK::Unary(HUnary::NegNum | HUnary::BitNotNum, x) => is(x, JK::Num),
            HK::Unary(HUnary::NotBool, x) => k == JK::Bool && self.cond(f, x),
            HK::Unary(HUnary::Truthy, x) => k == JK::Bool && self.cond(f, x),
            HK::Unary(HUnary::ToStr | HUnary::TypeOf, x) => k == JK::Val && fits(x, JK::Val),
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => fits(a, k) && fits(b, k),
            HK::Cond(c, a, b) => self.cond(f, c) && fits(a, k) && fits(b, k),
            HK::Assign(p, x) => self.assign_ok(f, p, x, k),
            HK::Compound(p, HBin::Num(_), x) => k == JK::Num && self.num_place(f, p) && is(x, JK::Num),
            HK::Update { place, .. } => k == JK::Num && self.num_place(f, place),
            HK::Compound(p, HBin::Concat, x) => concat_assign(p, x, &e.ty).is_some_and(|a| self.expr(f, &a) == Some(k)),
            HK::Cast(x) => fits(x, k),
            HK::Seq(xs) => !xs.is_empty() && xs.iter().all(|x| self.expr(f, x).is_some()) && fits(xs.last().unwrap(), k),
            HK::Call(c) => self.call(f, c, k),
            HK::New { class, args } => k == JK::Ref && (self.new_direct(f, *class, args) || self.val_args(f, args)),
            HK::NewBuiltin(_, args) => self.val_args(f, args),
            HK::Array(elems) => jv(&e.ty) == Some(JV::Arr) && elems.iter().all(|x| matches!(x, HElem::Expr(x) if self.expr(f, x).is_some())),
            _ => false,
        };
        ok.then_some(k)
    }

    /// `new C(…)` calling the compiled constructor directly.
    fn new_direct(&self, f: FuncId, class: u32, args: &[HElem]) -> bool {
        let ctor = self.hir.classes[class as usize].ctor;
        let hc = &self.hir.funcs[ctor as usize];
        self.set.contains(&ctor) && hc.this_var.is_some() && hc.free.is_empty() && self.args(f, ctor, args)
    }

    /// Can compiled code evaluate `x` and convert it to a `want`?
    fn fits(&self, f: FuncId, x: &HExpr, want: JK) -> bool {
        self.expr(f, x).is_some_and(|k| coercible(k, want))
    }

    /// Arguments passed as values (to the interpreter).
    fn val_args(&self, f: FuncId, args: &[HElem]) -> bool {
        args.iter().all(|a| matches!(a, HElem::Expr(x) if self.fits(f, x, JK::Val)))
    }

    /// An expression used only for its truth: `&&`/`||` may mix kinds.
    fn cond(&self, f: FuncId, e: &HExpr) -> bool {
        match &e.kind {
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => self.cond(f, a) && self.cond(f, b),
            HK::Unary(HUnary::Truthy, x) => self.cond(f, x),
            _ => self.expr(f, e).is_some(),
        }
    }

    /// A place compiled code can read as `k`. A field or element holds any
    /// value, so reads check its tag; a variable's value is converted.
    fn place(&self, f: FuncId, p: &Place, k: JK) -> bool {
        match p {
            Place::Var(v) => self.var_ok(f, *v) && coercible(self.vjk(*v).unwrap(), k),
            Place::Field { obj, opt: false, .. } => self.expr(f, obj) == Some(JK::Ref) && jv(&obj.ty) == Some(JV::Obj),
            Place::Index { obj, index, opt: false } => self.is_array(f, obj) && self.expr(f, index) == Some(JK::Num),
            Place::Prop { obj, opt: false, .. } => self.fits(f, obj, JK::Val),
            _ => false,
        }
    }

    /// A place holding a number, for `+=` and `++`.
    fn num_place(&self, f: FuncId, p: &Place) -> bool {
        match p {
            Place::Var(v) => self.var_ok(f, *v) && self.vjk(*v) == Some(JK::Num),
            Place::Field { .. } | Place::Index { .. } => self.place(f, p, JK::Num),
            _ => false,
        }
    }

    /// `p = x`, its result a `k`.
    fn assign_ok(&self, f: FuncId, p: &Place, x: &HExpr, k: JK) -> bool {
        let Some(xk) = self.expr(f, x) else { return false };
        let store = match p {
            Place::Var(v) => self.vjk(*v).unwrap(),
            _ => xk,
        };
        self.place(f, p, store) && coercible(xk, store) && coercible(store, k)
    }

    fn is_array(&self, f: FuncId, e: &HExpr) -> bool {
        jv(&e.ty) == Some(JV::Arr) && self.expr(f, e).is_some()
    }

    fn args(&self, f: FuncId, callee: FuncId, args: &[HElem]) -> bool {
        let hf = &self.hir.funcs[callee as usize];
        hf.params.len() == args.len()
            && hf.params.iter().zip(args).all(|(p, a)| match a {
                HElem::Expr(x) => self.fits(f, x, self.vjk(p.var).unwrap()),
                HElem::Spread(_) => false,
            })
    }

    /// The compiled function a call goes to directly, if any.
    fn direct_target(&self, c: &HCall) -> Option<FuncId> {
        let g = match &c.callee {
            // A function that captures nothing ignores its closure.
            Callee::Direct { func, closure } if closure.is_none() || self.hir.funcs[*func as usize].free.is_empty() => Some(*func),
            Callee::Method { class, vslot } if c.this.is_some() => self.devirt(*class, *vslot),
            _ => None,
        }?;
        let hg = &self.hir.funcs[g as usize];
        let this_ok = c.this.is_none() || hg.this_var.is_some();
        (self.set.contains(&g) && this_ok && hg.free.is_empty() && hg.params.len() == c.args.len()).then_some(g)
    }

    /// `arr.push(x)` compiled inline.
    fn inline_push(&self, f: FuncId, c: &HCall, k: JK) -> bool {
        matches!(c.callee, Callee::Native(Native::ArrPush))
            && k == JK::Num
            && c.this.as_ref().is_some_and(|t| self.is_array(f, t))
            && matches!(&c.args[..], [HElem::Expr(x)] if self.expr(f, x).is_some())
    }

    /// A `Math` call compiled inline: numbers in, a number out.
    fn inline_math(&self, f: FuncId, c: &HCall, n: Native, k: JK) -> bool {
        let one = matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(&n);
        let arity_ok = (one && c.args.len() == 1) || (MATH2.contains(&n) && c.args.len() == 2);
        c.this.is_none() && k == JK::Num && arity_ok && c.args.iter().all(|a| matches!(a, HElem::Expr(x) if self.expr(f, x) == Some(JK::Num)))
    }

    fn call(&self, f: FuncId, c: &HCall, k: JK) -> bool {
        if c.opt {
            return false;
        }
        if let Some(g) = self.direct_target(c) {
            let hg = &self.hir.funcs[g as usize];
            let this_ok = c.this.as_ref().is_none_or(|t| hg.this_var.is_some_and(|tv| self.fits(f, t, self.vjk(tv).unwrap())));
            return this_ok && coercible(ret_jk(hg).unwrap(), k) && self.args(f, g, &c.args);
        }
        let this_ok = c.this.as_ref().is_none_or(|t| self.fits(f, t, JK::Val));
        match &c.callee {
            Callee::Native(Native::ArrPush) if self.inline_push(f, c, k) => true,
            Callee::Native(n) if self.inline_math(f, c, *n, k) => true,
            // Through the interpreter: anything else it can call.
            Callee::Native(_) | Callee::Prop(_) => this_ok && self.val_args(f, &c.args),
            Callee::Value(fv) => this_ok && self.fits(f, fv, JK::Val) && self.val_args(f, &c.args),
            Callee::Direct { func, closure } => {
                let free = !self.hir.funcs[*func as usize].free.is_empty();
                let closure_ok = match closure {
                    Some(v) if free => self.var_ok(f, *v),
                    _ => !free,
                };
                closure_ok && this_ok && self.val_args(f, &c.args)
            }
            Callee::Method { .. } => c.this.as_ref().is_some_and(|t| self.fits(f, t, JK::Val)) && self.val_args(f, &c.args),
        }
    }
}

/// `p += x` on strings as `p = p + x`, when evaluating `p`'s object twice
/// is the same as once (a variable, or fields of one).
fn concat_assign(p: &Place, x: &HExpr, ty: &Type) -> Option<HExpr> {
    fn plain(e: &HExpr) -> bool {
        match &e.kind {
            HK::Read(p) => match &**p {
                Place::Var(_) => true,
                Place::Field { obj, opt: false, .. } => plain(obj),
                _ => false,
            },
            HK::Cast(y) => plain(y),
            _ => false,
        }
    }
    let ok = match p {
        Place::Var(_) => true,
        Place::Field { obj, opt: false, .. } => plain(obj),
        _ => false,
    };
    ok.then(|| {
        let cur = HExpr { kind: HK::Read(Box::new(p.clone())), ty: ty.clone() };
        let sum = HExpr { kind: HK::Binary(HBin::Concat, Box::new(cur), Box::new(x.clone())), ty: ty.clone() };
        HExpr { kind: HK::Assign(Box::new(p.clone()), Box::new(sum)), ty: ty.clone() }
    })
}

/// `throw new C("…", …)` with string literal arguments: the class and arguments.
fn throw_site(e: &HExpr) -> Option<(u32, Vec<std::rc::Rc<str>>)> {
    let HK::New { class, args } = &e.kind else { return None };
    let strs = args
        .iter()
        .map(|a| match a {
            HElem::Expr(x) => match &strip_cast(x).kind {
                HK::Str(s) => Some(s.clone()),
                _ => None,
            },
            HElem::Spread(_) => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((*class, strs))
}

thread_local! {
    static WHY_FN: RefCell<String> = const { RefCell::new(String::new()) };
}

/// `RTS_JIT_WHY=*` explains every function; `RTS_JIT_WHY=name` one.
fn why_all() -> bool {
    std::env::var("RTS_JIT_WHY").is_ok_and(|n| n == "*")
}

fn why_wanted(name: &str) -> bool {
    std::env::var("RTS_JIT_WHY").is_ok_and(|n| n == "*" || n == name)
}

fn why_say(msg: String) {
    let f = WHY_FN.with(|w| w.borrow().clone());
    if why_all() {
        let m: String = msg.chars().take(110).collect();
        eprintln!("rts jit why\t{f}\t{m}");
    } else {
        eprintln!("rts jit: {f} not compiled: {msg}");
    }
}

/// Every path through `ss` ends in a `return`.
fn always_returns(ss: &[HStmt]) -> bool {
    match ss.last() {
        Some(HStmt::Return(_) | HStmt::Throw(_)) => true,
        Some(HStmt::Block(b, _)) => always_returns(b),
        Some(HStmt::If(_, t, e)) => always_returns(t) && always_returns(e),
        Some(HStmt::Labeled(_, s)) => always_returns(std::slice::from_ref(&**s)),
        _ => false,
    }
}

/// The expressions inside a place (its object, index).
fn place_children(p: &Place) -> Vec<&HExpr> {
    match p {
        Place::Var(_) => vec![],
        Place::Field { obj, .. } | Place::Prop { obj, .. } | Place::ArrLen { obj, .. } | Place::StrLen { obj, .. } | Place::Accessor { obj, .. } => vec![obj],
        Place::Index { obj, index, .. } => vec![obj, index],
        Place::DynIndex { obj, key, .. } => vec![obj, key],
    }
}

/// An expression's direct subexpressions, in evaluation order.
fn children(e: &HExpr) -> Vec<&HExpr> {
    fn elems(xs: &[HElem]) -> Vec<&HExpr> {
        xs.iter().map(|a| match a { HElem::Expr(x) | HElem::Spread(x) => x }).collect()
    }
    match &e.kind {
        HK::Read(p) | HK::Update { place: p, .. } => place_children(p),
        HK::Assign(p, x) | HK::Compound(p, _, x) | HK::LogicalAssign(p, _, x) => {
            let mut v = place_children(p);
            v.push(x);
            v
        }
        HK::Call(c) => {
            let callee = match &c.callee {
                Callee::Value(f) => Some(f),
                _ => None,
            };
            c.this.iter().chain(callee).chain(elems(&c.args)).collect()
        }
        HK::New { args, .. } | HK::Array(args) | HK::NewBuiltin(_, args) => elems(args),
        HK::Binary(_, a, b) | HK::Logical(_, a, b) => vec![a, b],
        HK::Cond(a, b, c) => vec![a, b, c],
        HK::Unary(_, x) | HK::Cast(x) | HK::OptChain(x) | HK::InstanceOf(x, _) => vec![x],
        HK::Seq(xs) => xs.iter().collect(),
        _ => vec![],
    }
}

/// Is `n` a `Math` function (which changes nothing)?
fn math_native(n: Native) -> bool {
    matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(&n) || MATH2.contains(&n)
}

/// Assignments (and initializations) of variables in `ss`, with their values.
/// A statement's own expressions and its nested statement lists.
fn parts(s: &HStmt) -> (Vec<&HExpr>, Vec<&[HStmt]>) {
    match s {
        HStmt::Expr(x) | HStmt::Return(Some(x)) | HStmt::Throw(x) | HStmt::Init(_, Some(x)) => (vec![x], vec![]),
        HStmt::Block(b, _) => (vec![], vec![b]),
        HStmt::If(c, t, f) => (vec![c], vec![t, f]),
        HStmt::Loop { test, body, update, .. } => (test.iter().chain(update.iter()).collect(), vec![body]),
        HStmt::ForOf { iter, body, .. } | HStmt::ForIn { obj: iter, body, .. } => (vec![iter], vec![body]),
        HStmt::Switch { disc, cases, .. } => {
            (std::iter::once(disc).chain(cases.iter().filter_map(|c| c.0.as_ref())).collect(), cases.iter().map(|c| c.1.as_slice()).collect())
        }
        HStmt::Labeled(_, s) => (vec![], vec![std::slice::from_ref(&**s)]),
        HStmt::Try { block, handler, finalizer, .. } => (vec![], std::iter::once(block.as_slice()).chain([handler.as_slice()]).chain(finalizer.as_deref()).collect()),
        HStmt::Init(_, None) | HStmt::MakeClosure(..) | HStmt::Break(_) | HStmt::Continue(_) | HStmt::Return(None) => (vec![], vec![]),
    }
}

/// Every `v = x` of a variable: assignments, initializers, and a `for-of`
/// variable taking `iter`'s elements (`(elem, iter)`).
fn collect_ref_assigns<'a>(ss: &'a [HStmt], out: &mut Vec<(VarId, &'a HExpr)>) {
    fn e<'a>(x: &'a HExpr, out: &mut Vec<(VarId, &'a HExpr)>) {
        if let HK::Assign(p, v) = &x.kind
            && let Place::Var(var) = &**p
        {
            out.push((*var, v));
        }
        for c in children(x) {
            e(c, out);
        }
    }
    for s in ss {
        match s {
            HStmt::Init(v, Some(x)) => out.push((*v, x)),
            HStmt::ForOf { elem, iter, .. } => out.push((*elem, iter)),
            _ => {}
        }
        let (xs, bodies) = parts(s);
        for x in xs {
            e(x, out);
        }
        for b in bodies {
            collect_ref_assigns(b, out);
        }
    }
}

/// A value a borrowed variable may hold: reachable from `set`'s variables.
fn safe_borrow(x: &HExpr, set: &FxHashSet<VarId>) -> bool {
    match &x.kind {
        HK::Null | HK::Undefined => true,
        HK::Cast(y) => safe_borrow(y, set),
        HK::Read(p) => match &**p {
            Place::Var(v) => set.contains(v),
            Place::Field { obj, .. } | Place::Index { obj, .. } => safe_borrow(obj, set),
            _ => false,
        },
        _ => false,
    }
}

/// A read of a local variable (which only an assignment can change: a
/// callee may change a global), or a constant.
fn is_var_read(hir: &HProgram, e: &HExpr) -> bool {
    match &e.kind {
        HK::Read(p) => matches!(**p, Place::Var(v) if hir.vars[v as usize].owner.is_some() && !hir.vars[v as usize].captured),
        HK::Cast(x) => is_var_read(hir, x),
        HK::Null | HK::Undefined | HK::Str(_) => true,
        _ => false,
    }
}

/// Variables assigned anywhere in `ss` (to find parameters that must own their value).
fn assigned_vars(ss: &[HStmt], out: &mut FxHashSet<VarId>) {
    fn e(x: &HExpr, out: &mut FxHashSet<VarId>) {
        if let HK::Assign(p, _) | HK::Compound(p, _, _) | HK::LogicalAssign(p, _, _) | HK::Update { place: p, .. } = &x.kind
            && let Place::Var(v) = &**p
        {
            out.insert(*v);
        }
        for c in children(x) {
            e(c, out);
        }
    }
    for s in ss {
        let (xs, bodies) = parts(s);
        for x in xs {
            e(x, out);
        }
        for b in bodies {
            assigned_vars(b, out);
        }
    }
}

// ───────────── lowering ─────────────

fn cl_ty(k: JK) -> types::Type {
    match k {
        JK::Num => types::F64,
        JK::Bool => types::I8,
        JK::Ref | JK::Val => types::I64,
    }
}

/// The machine words of a `k`: a value takes two (tag, payload).
fn cl_tys(k: JK) -> Vec<types::Type> {
    if k.wide() { vec![types::I64, types::I64] } else { vec![cl_ty(k)] }
}

struct Helpers {
    modf: FuncRef,
    pow: FuncRef,
    int32: FuncRef,
    math1: FuncRef,
    math2: FuncRef,
    new: FuncRef,
    dec: FuncRef,
    drop_val: FuncRef,
    throw: FuncRef,
    arr_get: FuncRef,
    arr_set: FuncRef,
    arr_len: FuncRef,
    arr_push: FuncRef,
    arr_new: FuncRef,
    throw_site: FuncRef,
    inc_val: FuncRef,
    call: FuncRef,
    get_prop: FuncRef,
    set_prop: FuncRef,
    op: FuncRef,
    new_cell: FuncRef,
    closure: FuncRef,
    throw_value: FuncRef,
    iter_array: FuncRef,
}

/// Layout facts compiled code relies on.
struct Layout {
    tag_undef: i64,
    tag_null: i64,
    tag_bool: i64,
    tag_num: i64,
    tag_obj: i64,
    tag_arr: i64,
    tag_func: i64,
    /// Tags from here on (`BigInt`, …) may own a box.
    tag_first_gc: i64,
    /// Where a cell's value is, from the start of its box.
    cell_value: i32,
    arr: Option<ArrLayout>,
}

impl Layout {
    fn new() -> Layout {
        let t = |v: Value| v.tag() as i64;
        let l = Layout {
            tag_undef: t(Value::Undef),
            tag_null: t(Value::Null),
            tag_bool: t(Value::Bool(false)),
            tag_num: t(Value::Num(0.0)),
            tag_obj: t(Value::Obj(Object::new(crate::runtime::Shape::new(vec![]), vec![], None))),
            tag_arr: t(Value::Arr(crate::runtime::Array::new_num(vec![]))),
            tag_func: t(Value::Func(Gc::new(Closure { func: 0, caps: Box::new([]), props: Default::default() }))),
            tag_first_gc: t(Value::BigInt(Gc::new(0))),
            cell_value: {
                let c = Gc::new(RefCell::new(Value::Undef));
                (c.as_ptr() as usize - c.header_ptr().as_ptr() as usize) as i32
            },
            arr: if std::env::var_os("RTS_JIT_NO_INLINE_ARRAYS").is_some() { None } else { ArrLayout::probe() },
        };
        assert!(l.tag_undef < l.tag_first_gc && l.tag_null < l.tag_first_gc && l.tag_bool < l.tag_first_gc && l.tag_num < l.tag_first_gc);
        l
    }
}

/// Where an array's elements are, as offsets from its box (`None`: unknown,
/// so every access calls a helper). `Vec`'s field order is not fixed by
/// Rust, so it is found by inspecting a vector.
#[derive(Clone, Copy)]
struct ArrLayout {
    /// The `ArrStore` variant byte (0: numbers, 1: values).
    variant: i32,
    data: i32,
    len: i32,
}

impl ArrLayout {
    fn probe() -> Option<ArrLayout> {
        use crate::runtime::{ArrStore, Array};
        const W: usize = std::mem::size_of::<usize>();
        if std::mem::size_of::<Vec<f64>>() != 3 * W || std::mem::size_of::<Vec<Value>>() != 3 * W {
            return None;
        }
        // Which word of a vector is the data pointer, and which the length?
        let words = |p: usize, len: usize, cap: usize, w: [usize; 3]| -> Option<(usize, usize)> {
            let pi = w.iter().position(|x| *x == p)?;
            let li = w.iter().position(|x| *x == len)?;
            let ci = w.iter().position(|x| *x == cap)?;
            (pi != li && li != ci && pi != ci).then_some((pi, li))
        };
        let mut v: Vec<f64> = Vec::with_capacity(7);
        v.extend([1.0, 2.0, 3.0]);
        let (p, l, c) = (v.as_ptr() as usize, v.len(), v.capacity());
        let w: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        let num = words(p, l, c, w)?;
        let mut v: Vec<Value> = Vec::with_capacity(5);
        v.extend([Value::Undef, Value::Null]);
        let (p, l, c) = (v.as_ptr() as usize, v.len(), v.capacity());
        let w: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        if words(p, l, c, w)? != num {
            return None;
        }
        // The store inside the array's box.
        let a = Array::new_num(vec![1.0]);
        let base = a.header_ptr().as_ptr() as usize;
        let store = a.store.as_ptr() as usize - base;
        // The variant byte and the vector's offset (`repr(C, u8)`).
        let tag = |s: &ArrStore| unsafe { *(s as *const ArrStore as *const u8) };
        if tag(&ArrStore::Num(vec![])) != 0 || tag(&ArrStore::Val(vec![])) != 1 {
            return None;
        }
        let vec_at = store + 8;
        Some(ArrLayout { variant: store as i32, data: (vec_at + num.0 * W) as i32, len: (vec_at + num.1 * W) as i32 })
    }
}

/// Static counts of the reference-count updates compiled code makes and
/// skips (`RTS_JIT_DEBUG`).
#[derive(Default)]
struct RcStats {
    incs: usize,
    decs: usize,
    borrowed: usize,
}

struct Lower<'a, 'b> {
    hir: &'a HProgram,
    el: &'a Elig<'a>,
    fid: FuncId,
    rk: JK,
    /// The result, in detail.
    rjv: JV,
    b: FunctionBuilder<'b>,
    /// Each variable's word (a value's payload).
    vars: FxHashMap<VarId, Variable>,
    /// A value variable's tag word.
    tvars: FxHashMap<VarId, Variable>,
    /// Captured variables this function declares: each holds its cell (a
    /// counted box address, or 1 before the cell is made).
    cells: FxHashMap<VarId, Variable>,
    /// Cells declared by a block this code has entered (see `HStmt::Init`).
    declared: FxHashSet<VarId>,
    /// The cells this function's closure captured (its `free` variables, in order).
    env: Option<CV>,
    /// Counted variables holding a counted reference (locals, and assigned parameters).
    owned: Vec<VarId>,
    /// Reference variables that hold borrowed values (see `Elig::borrowed_vars`).
    borrowed: FxHashSet<VarId>,
    ctx: CV,
    depth: CV,
    unwind: Block,
    /// Owned temporaries not yet consumed (payload, tag): released if the
    /// code unwinds.
    pending: Vec<(CV, Option<CV>)>,
    /// (label, break target, continue target, is a switch).
    loops: Vec<(Option<Name>, Block, Option<Block>, bool)>,
    /// Each `for-of`'s array (keyed by the statement's address): owned,
    /// or 1 when the loop isn't running.
    iters: FxHashMap<usize, Variable>,
    funcs: FxHashMap<FuncId, FuncRef>,
    h: Helpers,
    lay: &'a Layout,
    classes: &'a [std::rc::Rc<ClassRt>],
    /// Each global variable's index (in `Vm::global_scalars` or `global_values`, by kind).
    globals: &'a FxHashMap<VarId, u32>,
    stats: &'a mut RcStats,
}

/// A value in compiled code: one word (a number, boolean or reference), or
/// two for any value (`t`, its tag, and `v`, its payload). For a counted
/// kind, `owned` says whether this code holds a count on it.
#[derive(Clone, Copy)]
struct RV {
    v: CV,
    t: Option<CV>,
    owned: bool,
}

impl RV {
    fn new(v: CV) -> RV {
        RV { v, t: None, owned: false }
    }
    fn val(t: CV, v: CV) -> RV {
        RV { v, t: Some(t), owned: false }
    }
    fn words(self) -> Vec<CV> {
        match self.t {
            Some(t) => vec![t, self.v],
            None => vec![self.v],
        }
    }
    fn of_words(w: &[CV]) -> RV {
        match w {
            [t, v] => RV::val(*t, *v),
            [v] => RV::new(*v),
            _ => unreachable!(),
        }
    }
}

fn mf() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// `JitCtx` field offsets.
const CTX_RET_TAG: i32 = std::mem::offset_of!(JitCtx, ret_tag) as i32;
const CTX_GLOBAL_SCALARS: i32 = std::mem::offset_of!(JitCtx, global_scalars) as i32;
const CTX_GLOBAL_VALUES: i32 = std::mem::offset_of!(JitCtx, global_values) as i32;

impl Lower<'_, '_> {
    fn var(&mut self, v: VarId) -> Variable {
        if let Some(x) = self.vars.get(&v) {
            return *x;
        }
        let k = self.el.vjk(v).expect("compiled variable");
        let x = self.b.declare_var(cl_ty(k));
        let init = self.zero_of(k);
        self.b.def_var(x, init);
        self.vars.insert(v, x);
        if k.wide() {
            let t = self.b.declare_var(types::I64);
            let u = self.b.ins().iconst(types::I64, self.lay.tag_undef);
            self.b.def_var(t, u);
            self.tvars.insert(v, t);
        }
        x
    }

    /// A variable's value (borrowed).
    fn use_var(&mut self, v: VarId) -> RV {
        let x = self.var(v);
        let p = self.b.use_var(x);
        match self.tvars.get(&v).copied() {
            Some(t) => RV::val(self.b.use_var(t), p),
            None => RV::new(p),
        }
    }

    /// A global variable's value (borrowed).
    fn global_get(&mut self, v: VarId) -> RV {
        let i = self.globals[&v] as i32;
        let vj = var_jv(self.hir, v);
        match vj {
            JV::Num | JV::Bool => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_SCALARS);
                let w = self.b.ins().load(types::I64, mf(), base, i * 8);
                RV::new(if vj == JV::Num { self.b.ins().bitcast(types::F64, MemFlagsData::new(), w) } else { self.b.ins().ireduce(types::I8, w) })
            }
            _ => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_VALUES);
                self.load_slot(base, i * 16, vj, [ERR_UNBOX_NUM, ERR_UNBOX_BOOL, ERR_UNBOX_REF])
            }
        }
    }

    /// Store into a global variable (`r` owned, if counted), releasing the old value.
    fn global_set(&mut self, v: VarId, r: RV) {
        let i = self.globals[&v] as i32;
        let vj = var_jv(self.hir, v);
        match vj {
            JV::Num | JV::Bool => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_SCALARS);
                let w = if vj == JV::Num { self.b.ins().bitcast(types::I64, MemFlagsData::new(), r.v) } else { self.b.ins().uextend(types::I64, r.v) };
                self.b.ins().store(mf(), w, base, i * 8);
            }
            _ => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_VALUES);
                self.store_slot(base, i * 16, vj, r);
            }
        }
    }

    fn is_global(&self, v: VarId) -> bool {
        self.hir.vars[v as usize].owner.is_none()
    }

    /// A variable in memory (a global, or captured: in a cell), not a register.
    fn in_mem(&self, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner.is_none() || vi.captured
    }

    /// The address of a captured variable's cell (borrowed).
    fn cell_ptr(&mut self, v: VarId) -> CV {
        if let Some(x) = self.cells.get(&v).copied() {
            return self.b.use_var(x);
        }
        let i = self.hir.funcs[self.fid as usize].free.iter().position(|x| *x == v).expect("a captured variable");
        let env = self.env.expect("a closure's cells");
        self.b.ins().load(types::I64, mf(), env, (i * 8) as i32)
    }

    /// A variable's value (borrowed: a global's or cell's may change in any call).
    fn read_var(&mut self, v: VarId) -> RV {
        if self.is_global(v) {
            self.global_get(v)
        } else if self.in_mem(v) {
            let c = self.cell_ptr(v);
            let off = self.lay.cell_value;
            self.load_slot(c, off, var_jv(self.hir, v), [ERR_UNBOX_NUM, ERR_UNBOX_BOOL, ERR_UNBOX_REF])
        } else {
            self.use_var(v)
        }
    }

    /// Store into a global or captured variable (`r` owned, if counted), releasing the old value.
    fn mem_set(&mut self, v: VarId, r: RV) {
        if self.is_global(v) {
            self.global_set(v, r);
        } else {
            let c = self.cell_ptr(v);
            let off = self.lay.cell_value;
            self.store_slot(c, off, var_jv(self.hir, v), r);
        }
    }

    /// Give captured variable `v` (one this function declares) a new cell,
    /// holding undefined, releasing the old one.
    fn new_cell(&mut self, v: VarId) {
        let x = self.cells[&v];
        let old = self.b.use_var(x);
        let call = self.b.ins().call(self.h.new_cell, &[]);
        let c = self.b.inst_results(call)[0];
        self.b.def_var(x, c);
        self.dec(old);
    }

    /// A new closure of `g` (owned), capturing its cells from this function.
    fn make_closure(&mut self, g: FuncId) -> RV {
        use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
        let free = self.hir.funcs[g as usize].free.clone();
        let n = free.len();
        let cells = if n > 0 {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (8 * n) as u32, 3));
            self.b.ins().stack_addr(types::I64, slot, 0)
        } else {
            self.b.ins().iconst(types::I64, 0)
        };
        for (i, v) in free.iter().enumerate() {
            let c = self.cell_ptr(*v);
            self.b.ins().store(mf(), c, cells, (8 * i) as i32);
        }
        let gi = self.b.ins().iconst(types::I32, g as i64);
        let ni = self.b.ins().iconst(types::I32, n as i64);
        let call = self.b.ins().call(self.h.closure, &[gi, cells, ni]);
        let p = self.b.inst_results(call)[0];
        let t = self.b.ins().iconst(types::I64, self.lay.tag_func);
        self.fresh(RV::val(t, p))
    }

    fn def_var(&mut self, v: VarId, r: RV) {
        let x = self.var(v);
        self.b.def_var(x, r.v);
        if let Some(t) = self.tvars.get(&v).copied() {
            self.b.def_var(t, r.t.expect("a value"));
        }
    }

    /// `undefined` (NaN, false) as a `k`'s single or payload word.
    fn zero_of(&mut self, k: JK) -> CV {
        match k {
            JK::Num => self.b.ins().f64const(f64::NAN),
            JK::Bool => self.b.ins().iconst(types::I8, 0),
            JK::Ref => self.b.ins().iconst(types::I64, 1),
            JK::Val => self.b.ins().iconst(types::I64, 0),
        }
    }

    fn zero_rv(&mut self, k: JK) -> RV {
        let v = self.zero_of(k);
        if k.wide() {
            let t = self.b.ins().iconst(types::I64, self.lay.tag_undef);
            RV::val(t, v)
        } else {
            RV::new(v)
        }
    }

    /// Code after a jump, return or break is unreachable: give it a block of its own.
    fn dead_block(&mut self) {
        let blk = self.b.create_block();
        self.b.switch_to_block(blk);
        self.b.seal_block(blk);
    }

    /// Branch to a new block if `cond`, run `then` there, and continue after.
    fn if_then(&mut self, cond: CV, then: impl FnOnce(&mut Self)) {
        let (tb, done) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(cond, tb, &[], done, &[]);
        self.b.seal_block(tb);
        self.b.switch_to_block(tb);
        then(self);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    // ── reference counts ──

    #[cfg(feature = "profile")]
    fn count_rc(&mut self) {
        let a = self.b.ins().iconst(types::I64, heap::STATS.rc_ops.as_ptr() as i64);
        let n = self.b.ins().load(types::I64, mf(), a, 0);
        let n1 = self.b.ins().iadd_imm_s(n, 1);
        self.b.ins().store(mf(), n1, a, 0);
    }
    #[cfg(not(feature = "profile"))]
    fn count_rc(&mut self) {}

    /// Increment a box's count, inline.
    fn inc_box(&mut self, p: CV) {
        let rc = self.b.ins().load(types::I32, mf(), p, 0);
        let rc1 = self.b.ins().iadd_imm_s(rc, 1);
        self.b.ins().store(mf(), rc1, p, 0);
        self.count_rc();
    }

    /// Take a reference (`p` may be null or undefined).
    fn inc(&mut self, p: CV) {
        self.stats.incs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, p, 2);
        self.if_then(real, |s| s.inc_box(p));
    }

    /// Take a reference to a value given as words: inline for an object or
    /// array, through a helper for other boxed kinds.
    fn inc_val(&mut self, t: CV, p: CV) {
        self.stats.incs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let boxed = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, t, self.lay.tag_first_gc);
        let (tag_obj, tag_arr) = (self.lay.tag_obj, self.lay.tag_arr);
        self.if_then(boxed, |s| {
            let is_obj = s.b.ins().icmp_imm_u(IntCC::Equal, t, tag_obj);
            let is_arr = s.b.ins().icmp_imm_u(IntCC::Equal, t, tag_arr);
            let is_box = s.b.ins().bor(is_obj, is_arr);
            let (ob, other, done) = (s.b.create_block(), s.b.create_block(), s.b.create_block());
            s.b.ins().brif(is_box, ob, &[], other, &[]);
            s.b.seal_block(ob);
            s.b.seal_block(other);
            s.b.switch_to_block(ob);
            s.inc_box(p);
            s.b.ins().jump(done, &[]);
            s.b.switch_to_block(other);
            s.b.ins().call(s.h.inc_val, &[t, p]);
            s.b.ins().jump(done, &[]);
            s.b.seal_block(done);
            s.b.switch_to_block(done);
        });
    }

    /// Release a reference (`p` may be null or undefined).
    fn dec(&mut self, p: CV) {
        self.stats.decs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, p, 2);
        self.if_then(real, |s| s.dec_box(p));
    }

    fn inc_rv(&mut self, r: RV) {
        match r.t {
            Some(t) => self.inc_val(t, r.v),
            None => self.inc(r.v),
        }
    }

    fn dec_rv(&mut self, r: RV) {
        match r.t {
            Some(t) => {
                self.stats.decs += 1;
                self.release_old(t, r.v)
            }
            None => self.dec(r.v),
        }
    }

    /// Release a reference to a box: inline unless the count may reach zero
    /// or the box may become a cycle candidate.
    fn dec_box(&mut self, p: CV) {
        let rc = self.b.ins().load(types::I32, mf(), p, 0);
        let fl = self.b.ins().load(types::I32, mf(), p, 4);
        let mask = (heap::flag::ACYCLIC | heap::flag::BUFFERED | heap::flag::ARENA) as i64;
        let m = self.b.ins().band_imm_u(fl, mask);
        let quiet = self.b.ins().icmp_imm_u(IntCC::NotEqual, m, 0);
        let above1 = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, rc, 1);
        let fast = self.b.ins().band(quiet, above1);
        let (fb, sb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(fast, fb, &[], sb, &[]);
        self.b.seal_block(fb);
        self.b.seal_block(sb);
        self.b.switch_to_block(fb);
        let rc1 = self.b.ins().iadd_imm_s(rc, -1);
        self.b.ins().store(mf(), rc1, p, 0);
        self.count_rc();
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(sb);
        self.b.ins().call(self.h.dec, &[p]);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    /// A new owned temporary.
    fn fresh(&mut self, r: RV) -> RV {
        self.pending.push((r.v, r.t));
        RV { owned: true, ..r }
    }

    fn unpend(&mut self, r: RV) {
        let i = self.pending.iter().rposition(|p| *p == (r.v, r.t)).expect("pending temporary");
        self.pending.remove(i);
    }

    /// `old` became `new` (a conversion): a pending temporary follows.
    fn repend(&mut self, old: RV, new: RV) {
        if !old.owned {
            return;
        }
        if let Some(i) = self.pending.iter().rposition(|p| *p == (old.v, old.t)) {
            if new.owned {
                self.pending[i] = (new.v, new.t);
            } else {
                self.pending.remove(i);
            }
        }
    }

    /// An owned copy of `r`, handed to the caller (who stores it at once).
    fn own(&mut self, r: RV) -> RV {
        if r.owned {
            self.unpend(r);
        } else {
            self.inc_rv(r);
        }
        RV { owned: true, ..r }
    }

    /// `r`, owned while other code runs.
    fn hold(&mut self, r: RV) -> RV {
        if r.owned {
            return r;
        }
        self.inc_rv(r);
        self.fresh(r)
    }

    /// Done with `r`.
    fn release(&mut self, r: RV) {
        if r.owned {
            self.unpend(r);
            self.dec_rv(r);
        } else {
            self.stats.borrowed += 1;
        }
    }

    /// Leave for the unwind block, releasing the pending temporaries.
    fn bail(&mut self) {
        for (v, t) in self.pending.clone() {
            self.dec_rv(RV { v, t, owned: true });
        }
        self.b.ins().jump(self.unwind, &[]);
    }

    /// Release every owned variable (except `keep`) and cell.
    fn release_vars(&mut self, keep: Option<VarId>) {
        for v in self.owned.clone() {
            if Some(v) != keep {
                let r = self.use_var(v);
                self.dec_rv(r);
            }
        }
        let mut cells: Vec<(VarId, Variable)> = self.cells.iter().map(|(v, x)| (*v, *x)).collect();
        cells.sort_by_key(|c| c.0);
        for (_, x) in cells {
            let c = self.b.use_var(x);
            self.dec(c);
        }
        let mut iters: Vec<(usize, Variable)> = self.iters.iter().map(|(k, x)| (*k, *x)).collect();
        iters.sort_by_key(|c| c.0);
        for (_, x) in iters {
            let a = self.b.use_var(x);
            self.dec(a);
        }
    }

    fn ret(&mut self, r: RV) {
        self.b.ins().store(mf(), self.depth, self.ctx, 0);
        self.b.ins().return_(&r.words());
    }

    /// Unwind if a callee or helper set the status.
    fn check_status(&mut self) {
        let st = self.b.ins().load(types::I32, mf(), self.ctx, 4);
        if self.pending.is_empty() {
            let ok = self.b.create_block();
            self.b.ins().brif(st, self.unwind, &[], ok, &[]);
            self.b.seal_block(ok);
            self.b.switch_to_block(ok);
            return;
        }
        let (bad, ok) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(st, bad, &[], ok, &[]);
        self.b.seal_block(bad);
        self.b.seal_block(ok);
        self.b.switch_to_block(bad);
        self.bail();
        self.b.switch_to_block(ok);
    }

    /// Throw a type error (unless `cond` is false) and unwind.
    fn throw_if(&mut self, cond: CV, code: u32, a: CV, b: CV) {
        let (tb, ok) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(cond, tb, &[], ok, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(ok);
        self.b.switch_to_block(tb);
        let c = self.b.ins().iconst(types::I32, code as i64);
        self.b.ins().call(self.h.throw, &[c, a, b]);
        self.bail();
        self.b.switch_to_block(ok);
    }

    /// Check that `p` is an object (not null or undefined).
    fn check_obj(&mut self, p: CV, code: u32) {
        let nullish = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, p, 2);
        let z = self.b.ins().iconst(types::I64, 0);
        self.throw_if(nullish, code, p, z);
    }

    // ── statements ──

    fn stmts(&mut self, ss: &[HStmt]) {
        for s in ss {
            self.stmt(s);
            debug_assert!(self.pending.is_empty(), "temporaries outlive a statement");
        }
    }

    fn stmt(&mut self, s: &HStmt) {
        match s {
            HStmt::Expr(e) => self.discard(e),
            HStmt::Init(v, e) if self.cells.contains_key(v) => {
                // A cell unless the enclosing block made one (as codegen does).
                if !self.declared.contains(v) {
                    self.new_cell(*v);
                }
                let vj = var_jv(self.hir, *v);
                let val = match e {
                    Some(e) => {
                        let r = self.value_as(e, vj);
                        if vj.jk().counted() { self.own(r) } else { r }
                    }
                    None => self.zero_rv(vj.jk()),
                };
                self.mem_set(*v, val);
            }
            HStmt::MakeClosure(v, g) => {
                let r = self.make_closure(*g);
                if self.cells.contains_key(v) {
                    if !self.declared.contains(v) {
                        self.new_cell(*v);
                    }
                    self.unpend(r);
                    self.mem_set(*v, RV { owned: true, ..r });
                } else {
                    self.unpend(r);
                    self.set_var(*v, RV { owned: true, ..r });
                }
            }
            HStmt::Init(v, e) => {
                let vj = var_jv(self.hir, *v);
                let val = match e {
                    Some(e) if self.borrowed.contains(v) => {
                        let r = self.value_as(e, vj);
                        debug_assert!(!r.owned);
                        self.stats.borrowed += 1;
                        r
                    }
                    Some(e) if vj.jk().counted() => {
                        let r = self.value_as(e, vj);
                        self.own(r)
                    }
                    Some(e) => self.value_as(e, vj),
                    None => self.zero_rv(vj.jk()),
                };
                self.set_var(*v, val);
            }
            HStmt::Block(ss, vars) => {
                for v in vars {
                    if self.cells.contains_key(v) {
                        self.new_cell(*v);
                        self.declared.insert(*v);
                    }
                }
                self.stmts(ss);
            }
            HStmt::If(c, t, e) => {
                let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                let cv = self.truthy(c);
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(eb);
                self.b.switch_to_block(tb);
                self.stmts(t);
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(eb);
                self.stmts(e);
                self.b.ins().jump(done, &[]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
            }
            HStmt::Loop { label, test, body, update, test_after, fresh } => {
                let (tb, bb, ub, xb) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.ins().jump(if *test_after { bb } else { tb }, &[]);
                self.b.switch_to_block(tb);
                match test {
                    Some(t) => {
                        let cv = self.truthy(t);
                        self.b.ins().brif(cv, bb, &[], xb, &[]);
                    }
                    None => {
                        self.b.ins().jump(bb, &[]);
                    }
                }
                self.b.switch_to_block(bb);
                self.loops.push((label.clone(), xb, Some(ub), false));
                self.stmts(body);
                self.loops.pop();
                self.b.ins().jump(ub, &[]);
                self.b.switch_to_block(ub);
                self.b.seal_block(ub);
                // Each iteration's closures keep their own copy of a captured loop variable.
                for v in fresh {
                    if let Some(x) = self.cells.get(v).copied() {
                        let old = self.b.use_var(x);
                        let r = self.load_slot(old, self.lay.cell_value, JV::Val, [0; 3]);
                        self.inc_rv(r);
                        let call = self.b.ins().call(self.h.new_cell, &[]);
                        let c = self.b.inst_results(call)[0];
                        self.b.def_var(x, c);
                        self.store_slot(c, self.lay.cell_value, JV::Val, r);
                        self.dec(old);
                    }
                }
                if let Some(u) = update {
                    self.discard(u);
                }
                self.b.ins().jump(tb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(bb);
                self.b.seal_block(xb);
                self.b.switch_to_block(xb);
            }
            HStmt::Labeled(name, s) if matches!(&**s, HStmt::ForOf { .. }) => self.for_of(s, Some(name.clone())),
            HStmt::Labeled(name, s) if !matches!(&**s, HStmt::Loop { .. }) => {
                let xb = self.b.create_block();
                self.loops.push((Some(name.clone()), xb, None, false));
                self.stmt(s);
                self.loops.pop();
                self.b.ins().jump(xb, &[]);
                self.b.seal_block(xb);
                self.b.switch_to_block(xb);
            }
            HStmt::Labeled(name, s) => {
                // `l: for (…)`: the label names the loop.
                if let HStmt::Loop { label: None, test, body, update, test_after, fresh } = &**s {
                    let l = HStmt::Loop { label: Some(name.clone()), test: test.clone(), body: body.clone(), update: update.clone(), test_after: *test_after, fresh: fresh.clone() };
                    self.stmt(&l);
                } else {
                    self.stmt(s);
                }
            }
            HStmt::Break(label) | HStmt::Continue(label) => {
                let is_break = matches!(s, HStmt::Break(_));
                let target = self.loops.iter().rev().find(|(l, _, c, sw)| match label {
                    Some(n) => l.as_ref() == Some(n),
                    None => c.is_some() || (is_break && *sw),
                });
                let (_, xb, cb, _) = target.expect("break target").clone();
                let to = if is_break { xb } else { cb.expect("continue target") };
                self.b.ins().jump(to, &[]);
                self.dead_block();
            }
            HStmt::Throw(e) if throw_site(e).is_none() => {
                let r = self.value_as(e, JV::Val);
                let r = self.own(r);
                self.b.ins().call(self.h.throw_value, &[r.t.unwrap(), r.v]);
                self.bail();
                self.dead_block();
            }
            HStmt::Switch { label, disc, cases } => self.switch(label, disc, cases),
            HStmt::ForOf { .. } => self.for_of(s, None),
            HStmt::Throw(e) => {
                let site = throw_site(e).unwrap();
                let i = THROW_SITES.with(|t| {
                    let mut t = t.borrow_mut();
                    t.push(site);
                    t.len() - 1
                });
                let i = self.b.ins().iconst(types::I32, i as i64);
                self.b.ins().call(self.h.throw_site, &[i]);
                self.bail();
                self.dead_block();
            }
            HStmt::Return(e) => {
                let r = match e {
                    None => {
                        self.release_vars(None);
                        self.zero_rv(self.rk)
                    }
                    Some(e) if self.rk.counted() => {
                        // Returning an owned local moves its reference out.
                        if let HK::Read(p) = &strip_cast(e).kind
                            && let Place::Var(v) = &**p
                            && self.owned.contains(v)
                            && self.el.vjk(*v) == Some(self.rk)
                            && self.rk == JK::Ref
                        {
                            let r = self.use_var(*v);
                            self.release_vars(Some(*v));
                            self.stats.borrowed += 1;
                            r
                        } else if let HK::Read(p) = &strip_cast(e).kind
                            && let Place::Var(v) = &**p
                            && self.owned.contains(v)
                            && self.el.vjk(*v) == Some(JK::Val)
                            && self.rjv == JV::Val
                        {
                            let r = self.use_var(*v);
                            self.release_vars(Some(*v));
                            self.stats.borrowed += 1;
                            r
                        } else {
                            let r = self.value_as(e, self.rjv);
                            let r = self.own(r);
                            self.release_vars(None);
                            r
                        }
                    }
                    Some(e) => {
                        let r = self.value_as(e, self.rjv);
                        self.release_vars(None);
                        r
                    }
                };
                self.ret(r);
                self.dead_block();
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// `switch`: the cases' tests in order (`===`), then the bodies, each
    /// falling through to the next.
    fn switch(&mut self, label: &Option<Name>, disc: &HExpr, cases: &[(Option<HExpr>, Vec<HStmt>)]) {
        let dj = jv(&disc.ty).unwrap();
        let d = self.value_of(disc);
        let var = is_var_read(self.hir, disc);
        let d = if dj.jk().counted() && !d.owned && cases.iter().any(|c| c.0.as_ref().is_some_and(|t| self.el.effects(t, var))) { self.hold(d) } else { d };
        let chosen = self.b.create_block();
        self.b.append_block_param(chosen, types::I32);
        for (k, (t, _)) in cases.iter().enumerate() {
            let Some(t) = t else { continue };
            let tj = jv(&t.ty).unwrap();
            let hit = match (dj, tj) {
                (JV::Num, JV::Num) => {
                    let y = self.expr(t);
                    self.b.ins().fcmp(FloatCC::Equal, d.v, y)
                }
                (JV::Bool, JV::Bool) => {
                    let y = self.expr(t);
                    self.b.ins().icmp(IntCC::Equal, d.v, y)
                }
                _ => {
                    let x = self.coerce(RV { owned: false, ..d }, dj, JV::Val);
                    let y = self.value_as(t, JV::Val);
                    let h = self.strict_eq(x, y);
                    self.release(y);
                    h
                }
            };
            let next = self.b.create_block();
            let kc = self.b.ins().iconst(types::I32, k as i64);
            self.b.ins().brif(hit, chosen, &[kc.into()], next, &[]);
            self.b.seal_block(next);
            self.b.switch_to_block(next);
        }
        let n = cases.len();
        let default = cases.iter().position(|c| c.0.is_none()).unwrap_or(n);
        let dc = self.b.ins().iconst(types::I32, default as i64);
        self.b.ins().jump(chosen, &[dc.into()]);
        self.b.seal_block(chosen);
        self.b.switch_to_block(chosen);
        let i = self.b.block_params(chosen)[0];
        self.release(d);
        let bodies: Vec<Block> = (0..n).map(|_| self.b.create_block()).collect();
        let xb = self.b.create_block();
        let mut sw = cranelift_frontend::Switch::new();
        for (k, blk) in bodies.iter().enumerate() {
            sw.set_entry(k as u128, *blk);
        }
        sw.emit(&mut self.b, i, xb);
        self.loops.push((label.clone(), xb, None, true));
        for (k, (_, body)) in cases.iter().enumerate() {
            self.b.switch_to_block(bodies[k]);
            self.b.seal_block(bodies[k]);
            self.stmts(body);
            self.b.ins().jump(*bodies.get(k + 1).unwrap_or(&xb), &[]);
        }
        self.loops.pop();
        self.b.seal_block(xb);
        self.b.switch_to_block(xb);
    }

    /// `for (x of iter)`: over an array as it is (its length read each
    /// time round), or over an array of another iterable's values.
    fn for_of(&mut self, s: &HStmt, outer: Option<Name>) {
        let HStmt::ForOf { label, elem, iter, body, .. } = s else { unreachable!() };
        let label = label.clone().or(outer);
        let held = self.iters[&(s as *const HStmt as usize)];
        let ij = jv(&iter.ty).unwrap();
        let a = if ij == JV::Arr {
            let r = self.value_as(iter, JV::Arr);
            let r = self.own(r);
            // null or undefined: the interpreter's error.
            let nullish = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, r.v, 2);
            let (t, p) = self.value_words(JV::Arr, r);
            self.if_then(nullish, |s| {
                s.b.ins().call(s.h.iter_array, &[t, p]);
            });
            self.check_status();
            r.v
        } else {
            let r = self.value_as(iter, JV::Val);
            let call = self.b.ins().call(self.h.iter_array, &[r.t.unwrap(), r.v]);
            let a = self.b.inst_results(call)[0];
            self.release(r);
            self.check_status();
            a
        };
        let old = self.b.use_var(held);
        self.b.def_var(held, a);
        self.dec(old);
        let iv = self.b.declare_var(types::F64);
        let zero = self.b.ins().f64const(0.0);
        self.b.def_var(iv, zero);
        let (tb, bb, ub, xb) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().jump(tb, &[]);
        self.b.switch_to_block(tb);
        let a = self.b.use_var(held);
        let len = self.arr_len(a);
        let i = self.b.use_var(iv);
        let more = self.b.ins().fcmp(FloatCC::LessThan, i, len);
        self.b.ins().brif(more, bb, &[], xb, &[]);
        self.b.switch_to_block(bb);
        let ej = var_jv(self.hir, *elem);
        let r = self.arr_get(a, i, ej);
        if self.cells.contains_key(elem) {
            // A new binding each time round.
            self.new_cell(*elem);
            let r = if ej.jk().counted() { self.own(r) } else { r };
            self.mem_set(*elem, r);
        } else if ej.jk().counted() && !self.borrowed.contains(elem) {
            let r = self.own(r);
            self.set_var(*elem, r);
        } else {
            self.set_var(*elem, r);
        }
        self.loops.push((label, xb, Some(ub), false));
        self.stmts(body);
        self.loops.pop();
        self.b.ins().jump(ub, &[]);
        self.b.switch_to_block(ub);
        self.b.seal_block(ub);
        let i = self.b.use_var(iv);
        let one = self.b.ins().f64const(1.0);
        let i1 = self.b.ins().fadd(i, one);
        self.b.def_var(iv, i1);
        self.b.ins().jump(tb, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(bb);
        self.b.seal_block(xb);
        self.b.switch_to_block(xb);
        let a = self.b.use_var(held);
        self.dec(a);
        let none = self.b.ins().iconst(types::I64, 1);
        self.b.def_var(held, none);
    }

    /// A variable for each `for-of`'s array, released with the variables.
    fn declare_iters(&mut self, ss: &[HStmt]) {
        for s in ss {
            if matches!(s, HStmt::ForOf { .. }) {
                let x = self.b.declare_var(types::I64);
                let none = self.b.ins().iconst(types::I64, 1);
                self.b.def_var(x, none);
                self.iters.insert(s as *const HStmt as usize, x);
            }
            for b in parts(s).1 {
                self.declare_iters(b);
            }
        }
    }

    /// Evaluate for effect.
    fn discard(&mut self, e: &HExpr) {
        if jk(&e.ty).unwrap().counted() {
            let r = self.rexpr(e);
            self.release(r);
        } else {
            self.expr(e);
        }
    }

    /// Store a value (owned, if counted) into a variable, releasing the old one.
    fn set_var(&mut self, v: VarId, val: RV) {
        if self.el.vjk(v).unwrap().counted() && !self.borrowed.contains(&v) {
            let old = self.use_var(v);
            self.def_var(v, val);
            self.dec_rv(old);
        } else {
            self.def_var(v, val);
        }
    }

    /// A condition as an i8 (0/1).
    fn truthy(&mut self, e: &HExpr) -> CV {
        match &e.kind {
            HK::Unary(HUnary::Truthy, x) => return self.truthy(x),
            HK::Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) => {
                let (rb, done) = (self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, types::I8);
                let ta = self.truthy(a);
                if *op == LogicalOp::And {
                    self.b.ins().brif(ta, rb, &[], done, &[ta.into()]);
                } else {
                    self.b.ins().brif(ta, done, &[ta.into()], rb, &[]);
                }
                self.b.seal_block(rb);
                self.b.switch_to_block(rb);
                let tb = self.truthy(b);
                self.b.ins().jump(done, &[tb.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                return self.b.block_params(done)[0];
            }
            _ => {}
        }
        match jk(&e.ty).unwrap() {
            k if k.counted() => {
                let r = self.rexpr(e);
                let t = self.truth_rv(r, k);
                self.release(r);
                t
            }
            k => {
                let v = self.expr(e);
                self.truth(v, k)
            }
        }
    }

    fn truth(&mut self, v: CV, k: JK) -> CV {
        match k {
            JK::Bool => v,
            JK::Num => {
                let z = self.b.ins().f64const(0.0);
                self.b.ins().fcmp(FloatCC::OrderedNotEqual, v, z)
            }
            JK::Ref => self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, v, 2),
            JK::Val => unreachable!("use truth_rv"),
        }
    }

    fn truth_rv(&mut self, r: RV, k: JK) -> CV {
        match r.t {
            Some(t) => self.truth_val(t, r.v),
            None => self.truth(r.v, k),
        }
    }

    /// A value's truth: inline for undefined, null, booleans, numbers,
    /// objects and arrays; through the helper for the rest (strings, …).
    fn truth_val(&mut self, t: CV, p: CV) -> CV {
        let l = self.lay;
        let (sb, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I8);
        let low = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, t, l.tag_first_gc);
        let is_obj = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_obj);
        let is_arr = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_arr);
        let is_box = self.b.ins().bor(is_obj, is_arr);
        let simple = self.b.ins().bor(low, is_box);
        self.b.ins().brif(simple, sb, &[], slow, &[]);
        self.b.seal_block(sb);
        self.b.seal_block(slow);
        self.b.switch_to_block(sb);
        let low_byte = self.b.ins().band_imm_u(p, 0xff);
        let bool_t = self.b.ins().icmp_imm_u(IntCC::NotEqual, low_byte, 0);
        let f = self.b.ins().bitcast(types::F64, MemFlagsData::new(), p);
        let z = self.b.ins().f64const(0.0);
        let num_t = self.b.ins().fcmp(FloatCC::OrderedNotEqual, f, z);
        let is_bool = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_bool);
        let is_num = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_num);
        let r = self.b.ins().select(is_bool, bool_t, is_box);
        let r = self.b.ins().select(is_num, num_t, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(slow);
        let r = self.op(OP_TRUTHY, t, p, None);
        let r = self.b.ins().ireduce(types::I8, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// `h_op` on one or two values' words: its raw result.
    fn op(&mut self, op: u32, t: CV, p: CV, b: Option<RV>) -> CV {
        let o = self.b.ins().iconst(types::I32, op as i64);
        let (bt, bp) = match b {
            Some(r) => (r.t.unwrap(), r.v),
            None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
        };
        let c = self.b.ins().call(self.h.op, &[o, t, p, bt, bp]);
        self.b.inst_results(c)[0]
    }

    /// A value result of a helper (its payload `p`, its tag in the context): owned.
    fn helper_val(&mut self, p: CV) -> RV {
        let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
        let r = self.fresh(RV::val(t, p));
        self.check_status();
        r
    }

    fn int32(&mut self, v: CV) -> CV {
        let c = self.b.ins().call(self.h.int32, &[v]);
        self.b.inst_results(c)[0]
    }

    fn num_op(&mut self, op: NumOp, a: CV, b: CV) -> CV {
        let ins = self.b.ins();
        match op {
            NumOp::Add => ins.fadd(a, b),
            NumOp::Sub => ins.fsub(a, b),
            NumOp::Mul => ins.fmul(a, b),
            NumOp::Div => ins.fdiv(a, b),
            NumOp::Mod | NumOp::Exp => {
                let h = if op == NumOp::Mod { self.h.modf } else { self.h.pow };
                let c = self.b.ins().call(h, &[a, b]);
                self.b.inst_results(c)[0]
            }
            _ => {
                let (x, y) = (self.int32(a), self.int32(b));
                let r = match op {
                    NumOp::BitAnd => self.b.ins().band(x, y),
                    NumOp::BitOr => self.b.ins().bor(x, y),
                    NumOp::BitXor => self.b.ins().bxor(x, y),
                    NumOp::Shl => self.b.ins().ishl(x, y),
                    NumOp::Shr => self.b.ins().sshr(x, y),
                    _ => {
                        let u = self.b.ins().ushr(x, y);
                        let w = self.b.ins().uextend(types::I64, u);
                        return self.b.ins().fcvt_from_uint(types::F64, w);
                    }
                };
                self.b.ins().fcvt_from_sint(types::F64, r)
            }
        }
    }

    // ── conversions ──

    /// Convert `r` from a `from` to a `to`, as codegen's `coerce`: boxing,
    /// or unboxing with a check (a type error). A count carries over.
    fn coerce(&mut self, r: RV, from: JV, to: JV) -> RV {
        let (fk, tk) = (from.jk(), to.jk());
        if fk == tk {
            return r;
        }
        let l = self.lay;
        let out = match (fk, tk) {
            (_, JK::Val) => {
                let (t, p) = self.value_words(from, r);
                RV { v: p, t: Some(t), owned: r.owned }
            }
            (JK::Val, JK::Num) => {
                let t = r.t.unwrap();
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, t, l.tag_num);
                self.throw_if(bad, ERR_UNBOX_NUM, t, r.v);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), r.v))
            }
            (JK::Val, JK::Bool) => {
                let t = r.t.unwrap();
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, t, l.tag_bool);
                self.throw_if(bad, ERR_UNBOX_BOOL, t, r.v);
                RV::new(self.b.ins().ireduce(types::I8, r.v))
            }
            (JK::Val, JK::Ref) => {
                let t = r.t.unwrap();
                let heap = match to {
                    JV::Arr => l.tag_arr,
                    JV::Obj => l.tag_obj,
                    _ => -1,
                };
                let is_heap = self.b.ins().icmp_imm_u(IntCC::Equal, t, heap);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_null);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_undef);
                let any = self.b.ins().bor(is_heap, is_null);
                let any = self.b.ins().bor(any, is_undef);
                let bad = self.b.ins().bxor_imm_u(any, 1);
                self.throw_if(bad, ERR_UNBOX_REF, t, r.v);
                let zero = self.b.ins().iconst(types::I64, 0);
                let one = self.b.ins().iconst(types::I64, 1);
                let nullish = self.b.ins().select(is_null, zero, one);
                RV { v: self.b.ins().select(is_heap, r.v, nullish), t: None, owned: r.owned }
            }
            _ => unreachable!("not coercible: {from:?} to {to:?}"),
        };
        self.repend(r, out);
        out
    }

    /// `e`, whatever its kind (a reference or value as `rexpr` gives it).
    fn value_of(&mut self, e: &HExpr) -> RV {
        if jk(&e.ty).unwrap().counted() { self.rexpr(e) } else { RV::new(self.expr(e)) }
    }

    /// `e`, converted to a `to`.
    fn value_as(&mut self, e: &HExpr, to: JV) -> RV {
        let r = self.value_of(e);
        self.coerce(r, jv(&e.ty).unwrap(), to)
    }

    // ── fields ──

    /// The object of a field access, checked. Held across `rest` (code that
    /// runs before the access) it must be owned unless nothing in `rest`
    /// could free it.
    fn field_base(&mut self, obj: &HExpr, rest: Option<&HExpr>, code: u32) -> RV {
        let r = self.value_as(obj, JV::Obj);
        let r = match rest {
            Some(x) if !r.owned && (if is_var_read(self.hir, obj) { self.el.effects(x, true) } else { self.el.effects(x, false) }) => self.hold(r),
            _ => r,
        };
        self.check_obj(r.v, code);
        r
    }

    /// A value's tag and payload words (both i64).
    fn value_words(&mut self, vj: JV, r: RV) -> (CV, CV) {
        let v = r.v;
        match vj {
            JV::Num => (self.b.ins().iconst(types::I64, self.lay.tag_num), self.b.ins().bitcast(types::I64, MemFlagsData::new(), v)),
            JV::Bool => (self.b.ins().iconst(types::I64, self.lay.tag_bool), self.b.ins().uextend(types::I64, v)),
            JV::Val => (r.t.unwrap(), v),
            _ => {
                let heap = if vj == JV::Arr { self.lay.tag_arr } else { self.lay.tag_obj };
                let obj = self.b.ins().iconst(types::I64, heap);
                let null = self.b.ins().iconst(types::I64, self.lay.tag_null);
                let undef = self.b.ins().iconst(types::I64, self.lay.tag_undef);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, v, 0);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, v, 1);
                let t = self.b.ins().select(is_undef, undef, obj);
                (self.b.ins().select(is_null, null, t), v)
            }
        }
    }

    /// Check the words of a slot's value (at `at + off`) and convert to a
    /// `want` (borrowed). `codes` are the errors for a number, a boolean
    /// and a reference.
    fn load_slot(&mut self, at: CV, off: i32, want: JV, codes: [u32; 3]) -> RV {
        let tag = self.b.ins().load(types::I8, mf(), at, off);
        let tag = self.b.ins().uextend(types::I64, tag);
        let payload = self.b.ins().load(types::I64, mf(), at, off + 8);
        match want {
            JV::Val => RV::val(tag, payload),
            JV::Num => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_num);
                self.throw_if(bad, codes[0], tag, payload);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), payload))
            }
            JV::Bool => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_bool);
                self.throw_if(bad, codes[1], tag, payload);
                RV::new(self.b.ins().load(types::I8, mf(), at, off + 8))
            }
            _ => {
                let heap = match want {
                    JV::Arr => self.lay.tag_arr,
                    JV::Obj => self.lay.tag_obj,
                    _ => -1,
                };
                let is_heap = self.b.ins().icmp_imm_u(IntCC::Equal, tag, heap);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_null);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_undef);
                let any = self.b.ins().bor(is_heap, is_null);
                let any = self.b.ins().bor(any, is_undef);
                let bad = self.b.ins().bxor_imm_u(any, 1);
                self.throw_if(bad, codes[2], tag, payload);
                let zero = self.b.ins().iconst(types::I64, 0);
                let one = self.b.ins().iconst(types::I64, 1);
                let nullish = self.b.ins().select(is_null, zero, one);
                RV::new(self.b.ins().select(is_heap, payload, nullish))
            }
        }
    }

    /// Read field `slot` of the object `p` as `want` (borrowed).
    fn load_field(&mut self, p: CV, slot: u32, want: JV) -> RV {
        let off = Object::field_offset(slot as usize) as i32;
        self.load_slot(p, off, want, [ERR_FIELD_NUM, ERR_FIELD_BOOL, ERR_FIELD_REF])
    }

    /// Store `r` (a `vj`, owned if counted) into the slot at `at + off`,
    /// then release the old value.
    fn store_slot(&mut self, at: CV, off: i32, vj: JV, r: RV) {
        let old_tag = self.b.ins().load(types::I8, mf(), at, off);
        let old_tag = self.b.ins().uextend(types::I64, old_tag);
        let old = self.b.ins().load(types::I64, mf(), at, off + 8);
        let (tag, payload) = self.value_words(vj, r);
        self.b.ins().istore8(mf(), tag, at, off);
        self.b.ins().store(mf(), payload, at, off + 8);
        self.release_old(old_tag, old);
    }

    /// Store `r` (a `vj`, owned if counted) into field `slot` of `p`.
    fn store_field(&mut self, p: CV, slot: u32, vj: JV, r: RV) {
        let off = Object::field_offset(slot as usize) as i32;
        self.store_slot(p, off, vj, r);
    }

    /// Release a value overwritten in a slot (its tag, widened, and payload).
    fn release_old(&mut self, old_tag: CV, old: CV) {
        // Nothing to do unless it may own a box.
        let boxed = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, old_tag, self.lay.tag_first_gc);
        let (tag_obj, tag_arr) = (self.lay.tag_obj, self.lay.tag_arr);
        self.if_then(boxed, |s| {
            if cfg!(feature = "gc") {
                return;
            }
            let is_obj = s.b.ins().icmp_imm_u(IntCC::Equal, old_tag, tag_obj);
            let is_arr = s.b.ins().icmp_imm_u(IntCC::Equal, old_tag, tag_arr);
            let is_box = s.b.ins().bor(is_obj, is_arr);
            let (ob, other, done) = (s.b.create_block(), s.b.create_block(), s.b.create_block());
            s.b.ins().brif(is_box, ob, &[], other, &[]);
            s.b.seal_block(ob);
            s.b.seal_block(other);
            s.b.switch_to_block(ob);
            s.stats.decs += 1;
            s.dec_box(old);
            s.b.ins().jump(done, &[]);
            s.b.switch_to_block(other);
            s.b.ins().call(s.h.drop_val, &[old_tag, old]);
            s.b.ins().jump(done, &[]);
            s.b.seal_block(done);
            s.b.switch_to_block(done);
        });
    }

    // ── arrays ──

    /// The array of an element access; held (owned) if `rest` (evaluated
    /// before the access) could free it.
    fn arr_base(&mut self, obj: &HExpr, rest: &[&HExpr]) -> RV {
        let r = self.value_as(obj, JV::Arr);
        let var = is_var_read(self.hir, obj);
        if !r.owned && rest.iter().any(|x| self.el.effects(x, var)) { self.hold(r) } else { r }
    }

    /// `i` as an index into the array at `a` (a real array, `al` its
    /// layout), and whether it is an integer within bounds.
    fn arr_index(&mut self, a: CV, i: CV, al: ArrLayout) -> (CV, CV) {
        let ii = self.b.ins().fcvt_to_sint_sat(types::I64, i);
        let back = self.b.ins().fcvt_from_sint(types::F64, ii);
        let exact = self.b.ins().fcmp(FloatCC::Equal, back, i);
        let len = self.b.ins().load(types::I64, mf(), a, al.len);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, ii, len);
        (ii, self.b.ins().band(exact, inb))
    }

    /// Branch to `fast` if `a` is an array whose store variant is `variant`
    /// (any, if None) and `i` an index within it; else to `slow`. Returns
    /// the integer index (valid in `fast`).
    fn arr_guard(&mut self, a: CV, i: CV, al: ArrLayout, variant: Option<i64>, fast: Block, slow: Block) -> CV {
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, a, 2);
        let chk = self.b.create_block();
        self.b.ins().brif(real, chk, &[], slow, &[]);
        self.b.seal_block(chk);
        self.b.switch_to_block(chk);
        let (ii, ok) = self.arr_index(a, i, al);
        let ok = match variant {
            Some(v) => {
                let var = self.b.ins().load(types::I8, mf(), a, al.variant);
                let is = self.b.ins().icmp_imm_u(IntCC::Equal, var, v);
                self.b.ins().band(ok, is)
            }
            None => ok,
        };
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        ii
    }

    /// Read `a[i]` as `want` (borrowed): inline for an in-bounds integer
    /// index into the expected storage, else through the helper.
    fn arr_get(&mut self, a: CV, i: CV, want: JV) -> RV {
        let Some(al) = self.lay.arr else { return self.arr_get_slow(a, i, want) };
        if want == JV::Val {
            return self.arr_get_val(a, i, al);
        }
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        let ty = cl_ty(want.jk());
        self.b.append_block_param(done, ty);
        let ii = self.arr_guard(a, i, al, Some(if want == JV::Num { 0 } else { 1 }), fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        if want == JV::Num {
            let off = self.b.ins().ishl_imm_u(ii, 3);
            let at = self.b.ins().iadd(data, off);
            let v = self.b.ins().load(types::F64, mf(), at, 0);
            self.b.ins().jump(done, &[v.into()]);
        } else {
            let off = self.b.ins().ishl_imm_u(ii, 4);
            let at = self.b.ins().iadd(data, off);
            let tag = self.b.ins().load(types::I8, mf(), at, 0);
            let tag = self.b.ins().uextend(types::I64, tag);
            let payload = self.b.ins().load(types::I64, mf(), at, 8);
            let (v, good) = if want == JV::Bool {
                (self.b.ins().ireduce(types::I8, payload), self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_bool))
            } else {
                let heap = match want {
                    JV::Arr => self.lay.tag_arr,
                    JV::Obj => self.lay.tag_obj,
                    _ => -1,
                };
                let is_heap = self.b.ins().icmp_imm_u(IntCC::Equal, tag, heap);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_null);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_undef);
                let zero = self.b.ins().iconst(types::I64, 0);
                let one = self.b.ins().iconst(types::I64, 1);
                let nullish = self.b.ins().select(is_null, zero, one);
                let v = self.b.ins().select(is_heap, payload, nullish);
                let any = self.b.ins().bor(is_heap, is_null);
                (v, self.b.ins().bor(any, is_undef))
            };
            self.b.ins().brif(good, done, &[v.into()], slow, &[]);
        }
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let v = self.arr_get_slow(a, i, want);
        self.b.ins().jump(done, &[v.v.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        RV::new(self.b.block_params(done)[0])
    }

    /// Read `a[i]` as any value: inline from either storage.
    fn arr_get_val(&mut self, a: CV, i: CV, al: ArrLayout) -> RV {
        let (fast, nums, vals, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I64);
        self.b.append_block_param(done, types::I64);
        let ii = self.arr_guard(a, i, al, None, fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        let var = self.b.ins().load(types::I8, mf(), a, al.variant);
        self.b.ins().brif(var, vals, &[], nums, &[]);
        self.b.seal_block(nums);
        self.b.seal_block(vals);
        self.b.switch_to_block(nums);
        let off = self.b.ins().ishl_imm_u(ii, 3);
        let at = self.b.ins().iadd(data, off);
        let bits = self.b.ins().load(types::I64, mf(), at, 0);
        let tn = self.b.ins().iconst(types::I64, self.lay.tag_num);
        self.b.ins().jump(done, &[tn.into(), bits.into()]);
        self.b.switch_to_block(vals);
        let off = self.b.ins().ishl_imm_u(ii, 4);
        let at = self.b.ins().iadd(data, off);
        let tag = self.b.ins().load(types::I8, mf(), at, 0);
        let tag = self.b.ins().uextend(types::I64, tag);
        let payload = self.b.ins().load(types::I64, mf(), at, 8);
        self.b.ins().jump(done, &[tag.into(), payload.into()]);
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let r = self.arr_get_slow(a, i, JV::Val);
        self.b.ins().jump(done, &[r.t.unwrap().into(), r.v.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let p = self.b.block_params(done);
        RV::val(p[0], p[1])
    }

    /// `a[i] = r` (`r` a `vj`, owned if counted): inline for an in-bounds
    /// integer index (numbers into number storage, anything into value
    /// storage), else through the helper.
    fn arr_set(&mut self, a: CV, i: CV, vj: JV, r: RV) {
        let Some(al) = self.lay.arr else { return self.arr_set_slow(a, i, vj, r) };
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        // Non-numbers only go inline into value storage.
        let ii = self.arr_guard(a, i, al, if vj == JV::Num { None } else { Some(1) }, fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        let vals = self.b.create_block();
        if vj == JV::Num {
            let var = self.b.ins().load(types::I8, mf(), a, al.variant);
            let nums = self.b.create_block();
            self.b.ins().brif(var, vals, &[], nums, &[]);
            self.b.seal_block(nums);
            self.b.switch_to_block(nums);
            let off = self.b.ins().ishl_imm_u(ii, 3);
            let at = self.b.ins().iadd(data, off);
            self.b.ins().store(mf(), r.v, at, 0);
            self.b.ins().jump(done, &[]);
        } else {
            self.b.ins().jump(vals, &[]);
        }
        self.b.seal_block(vals);
        self.b.switch_to_block(vals);
        let off = self.b.ins().ishl_imm_u(ii, 4);
        let at = self.b.ins().iadd(data, off);
        self.store_slot(at, 0, vj, r);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        self.arr_set_slow(a, i, vj, r);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    /// `a.length`.
    fn arr_len(&mut self, a: CV) -> CV {
        let Some(al) = self.lay.arr else { return self.arr_len_slow(a) };
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::F64);
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, a, 2);
        self.b.ins().brif(real, fast, &[], slow, &[]);
        self.b.seal_block(fast);
        self.b.seal_block(slow);
        self.b.switch_to_block(fast);
        let n = self.b.ins().load(types::I64, mf(), a, al.len);
        let n = self.b.ins().fcvt_from_uint(types::F64, n);
        self.b.ins().jump(done, &[n.into()]);
        self.b.switch_to_block(slow);
        let n = self.arr_len_slow(a);
        self.b.ins().jump(done, &[n.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    fn arr_len_slow(&mut self, a: CV) -> CV {
        let call = self.b.ins().call(self.h.arr_len, &[a]);
        let n = self.b.inst_results(call)[0];
        self.check_status();
        n
    }

    /// Read `a[i]` as `want` (borrowed), through the helper.
    fn arr_get_slow(&mut self, a: CV, i: CV, want: JV) -> RV {
        let w = match want {
            JV::Num => W_NUM,
            JV::Bool => W_BOOL,
            JV::Obj => W_OBJ,
            JV::Arr => W_ARR,
            JV::Nil => W_NIL,
            JV::Val => W_VAL,
        };
        let w = self.b.ins().iconst(types::I32, w as i64);
        let call = self.b.ins().call(self.h.arr_get, &[a, i, w]);
        let r = self.b.inst_results(call)[0];
        let t = (want == JV::Val).then(|| self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG));
        self.check_status();
        match want {
            JV::Num => RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), r)),
            JV::Bool => RV::new(self.b.ins().ireduce(types::I8, r)),
            JV::Val => RV::val(t.unwrap(), r),
            _ => RV::new(r),
        }
    }

    /// `a[i] = r`, through the helper.
    fn arr_set_slow(&mut self, a: CV, i: CV, vj: JV, r: RV) {
        let (tag, payload) = self.value_words(vj, r);
        let num = self.b.ins().iconst(types::I32, (vj == JV::Num) as i64);
        self.b.ins().call(self.h.arr_set, &[a, i, tag, payload, num]);
        self.check_status();
    }

    /// An owned value of `x` (a reference's count taken), as words for a helper.
    fn owned_words(&mut self, x: &HExpr) -> (CV, CV) {
        let vj = jv(&x.ty).unwrap();
        let r = self.value_of(x);
        let r = if vj.jk().counted() { self.own(r) } else { r };
        self.value_words(vj, r)
    }

    // ── expressions ──

    /// A number or boolean expression.
    fn expr(&mut self, e: &HExpr) -> CV {
        let k = jk(&e.ty).expect("checked by Elig");
        let want = jv(&e.ty).unwrap();
        if k.counted() {
            unreachable!("counted expressions go through rexpr");
        }
        match &e.kind {
            HK::Num(x) => self.b.ins().f64const(*x),
            HK::Bool(x) => self.b.ins().iconst(types::I8, *x as i64),
            HK::Read(p) => self.read(p, &e.ty).v,
            HK::Binary(HBin::Num(op), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                self.num_op(*op, x, y)
            }
            HK::Binary(HBin::NumCmp(op), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                let cc = match op {
                    CmpOp::Lt => FloatCC::LessThan,
                    CmpOp::Le => FloatCC::LessThanOrEqual,
                    CmpOp::Gt => FloatCC::GreaterThan,
                    CmpOp::Ge => FloatCC::GreaterThanOrEqual,
                    CmpOp::Eq => FloatCC::Equal,
                    CmpOp::Ne => FloatCC::NotEqual,
                };
                self.b.ins().fcmp(cc, x, y)
            }
            HK::Binary(HBin::BoolEq(eq), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                self.b.ins().icmp(if *eq { IntCC::Equal } else { IntCC::NotEqual }, x, y)
            }
            HK::Binary(op @ (HBin::StrictEq(eq) | HBin::LooseEq(eq)), a, b) => {
                let loose = matches!(op, HBin::LooseEq(_));
                let (ka, kb) = (jk(&a.ty).unwrap(), jk(&b.ty).unwrap());
                if ka != kb || ka == JK::Val {
                    return self.val_eq(a, b, loose, *eq);
                }
                match ka {
                    JK::Num => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().fcmp(if *eq { FloatCC::Equal } else { FloatCC::NotEqual }, x, y)
                    }
                    JK::Bool => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().icmp(if *eq { IntCC::Equal } else { IntCC::NotEqual }, x, y)
                    }
                    _ => {
                        // Only identity is compared: a borrowed left side
                        // must stay alive (and unchanged) while the right runs.
                        let x = self.rexpr(a);
                        let x = if !x.owned && self.el.effects(b, is_var_read(self.hir, a)) { self.hold(x) } else { x };
                        let y = self.rexpr(b);
                        let same = self.b.ins().icmp(IntCC::Equal, x.v, y.v);
                        let same = if loose {
                            // null == undefined
                            let xn = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, x.v, 2);
                            let yn = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, y.v, 2);
                            let both = self.b.ins().band(xn, yn);
                            self.b.ins().bor(same, both)
                        } else {
                            same
                        };
                        self.release(x);
                        self.release(y);
                        if *eq { same } else { self.b.ins().bxor_imm_u(same, 1) }
                    }
                }
            }
            HK::Binary(HBin::StrCmp(op), a, b) => {
                let x = self.value_as(a, JV::Val);
                let x = if !x.owned && self.el.effects(b, is_var_read(self.hir, a)) { self.hold(x) } else { x };
                let y = self.value_as(b, JV::Val);
                let r = self.op(OP_STR_CMP + *op as u32, x.t.unwrap(), x.v, Some(y));
                self.release(x);
                self.release(y);
                self.b.ins().ireduce(types::I8, r)
            }
            HK::Unary(HUnary::NegNum, x) => {
                let v = self.expr(x);
                self.b.ins().fneg(v)
            }
            HK::Unary(HUnary::BitNotNum, x) => {
                let v = self.expr(x);
                let i = self.int32(v);
                let n = self.b.ins().bnot(i);
                self.b.ins().fcvt_from_sint(types::F64, n)
            }
            HK::Unary(HUnary::NotBool, x) => {
                let v = self.truthy(x);
                self.b.ins().bxor_imm_u(v, 1)
            }
            HK::Unary(HUnary::Truthy, x) => self.truthy(x),
            HK::Logical(op, a, b) => self.logical(*op, a, b, want).v,
            HK::Cond(c, a, b) => self.cond_expr(c, a, b, want).v,
            HK::Assign(p, x) => self.assign(p, x, want).v,
            HK::Compound(p, HBin::Num(op), x) => match &**p {
                Place::Var(v) if self.in_mem(*v) => {
                    let cur = self.read_var(*v).v;
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.mem_set(*v, RV::new(r));
                    r
                }
                Place::Var(v) => {
                    let var = self.var(*v);
                    let cur = self.b.use_var(var);
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.b.def_var(var, r);
                    r
                }
                Place::Field { obj, slot, .. } => {
                    let o = self.field_base(obj, Some(x), ERR_READ);
                    let cur = self.load_field(o.v, *slot, JV::Num).v;
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.store_field(o.v, *slot, JV::Num, RV::new(r));
                    self.release(o);
                    r
                }
                Place::Index { obj, index, .. } => {
                    let a = self.arr_base(obj, &[index, x]);
                    let i = self.expr(index);
                    let cur = self.arr_get(a.v, i, JV::Num).v;
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.arr_set(a.v, i, JV::Num, RV::new(r));
                    self.release(a);
                    r
                }
                _ => unreachable!(),
            },
            HK::Update { place, inc, prefix } => {
                let one = self.b.ins().f64const(1.0);
                let step = |s: &mut Self, cur: CV| if *inc { s.b.ins().fadd(cur, one) } else { s.b.ins().fsub(cur, one) };
                match &**place {
                    Place::Var(v) if self.in_mem(*v) => {
                        let cur = self.read_var(*v).v;
                        let r = step(self, cur);
                        self.mem_set(*v, RV::new(r));
                        if *prefix { r } else { cur }
                    }
                    Place::Var(v) => {
                        let var = self.var(*v);
                        let cur = self.b.use_var(var);
                        let r = step(self, cur);
                        self.b.def_var(var, r);
                        if *prefix { r } else { cur }
                    }
                    Place::Field { obj, slot, .. } => {
                        let o = self.field_base(obj, None, ERR_READ);
                        let cur = self.load_field(o.v, *slot, JV::Num).v;
                        let r = step(self, cur);
                        self.store_field(o.v, *slot, JV::Num, RV::new(r));
                        self.release(o);
                        if *prefix { r } else { cur }
                    }
                    Place::Index { obj, index, .. } => {
                        let a = self.arr_base(obj, &[index]);
                        let i = self.expr(index);
                        let cur = self.arr_get(a.v, i, JV::Num).v;
                        let r = step(self, cur);
                        self.arr_set(a.v, i, JV::Num, RV::new(r));
                        self.release(a);
                        if *prefix { r } else { cur }
                    }
                    _ => unreachable!(),
                }
            }
            HK::Cast(x) => self.value_as(x, want).v,
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.value_as(xs.last().unwrap(), want).v
            }
            HK::Call(c) => self.call(c, want).v,
            _ => unreachable!("checked by Elig"),
        }
    }

    /// `a === b` / `a == b` (or `!==`, `!=`) on values.
    fn val_eq(&mut self, a: &HExpr, b: &HExpr, loose: bool, eq: bool) -> CV {
        let x = self.value_as(a, JV::Val);
        let x = if !x.owned && self.el.effects(b, is_var_read(self.hir, a)) { self.hold(x) } else { x };
        let y = self.value_as(b, JV::Val);
        let r = if loose {
            let r = self.op(OP_LOOSE_EQ, x.t.unwrap(), x.v, Some(y));
            self.b.ins().ireduce(types::I8, r)
        } else {
            self.strict_eq(x, y)
        };
        self.release(x);
        self.release(y);
        if eq { r } else { self.b.ins().bxor_imm_u(r, 1) }
    }

    /// `===` on two values' words: inline unless both are strings (or
    /// another kind compared by contents).
    fn strict_eq(&mut self, x: RV, y: RV) -> CV {
        let l = self.lay;
        let (xt, yt) = (x.t.unwrap(), y.t.unwrap());
        let (same, simple_b, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I8);
        let same_tag = self.b.ins().icmp(IntCC::Equal, xt, yt);
        let f = self.b.ins().iconst(types::I8, 0);
        self.b.ins().brif(same_tag, same, &[], done, &[f.into()]);
        self.b.seal_block(same);
        self.b.switch_to_block(same);
        let is_num = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_num);
        let is_bool = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_bool);
        let is_obj = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_obj);
        let is_arr = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_arr);
        let is_ptr = self.b.ins().bor(is_obj, is_arr);
        let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_null);
        let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_undef);
        let nullish = self.b.ins().bor(is_null, is_undef);
        let s1 = self.b.ins().bor(is_num, is_bool);
        let s2 = self.b.ins().bor(is_ptr, nullish);
        let simple = self.b.ins().bor(s1, s2);
        self.b.ins().brif(simple, simple_b, &[], slow, &[]);
        self.b.seal_block(simple_b);
        self.b.seal_block(slow);
        self.b.switch_to_block(simple_b);
        let xf = self.b.ins().bitcast(types::F64, MemFlagsData::new(), x.v);
        let yf = self.b.ins().bitcast(types::F64, MemFlagsData::new(), y.v);
        let num_eq = self.b.ins().fcmp(FloatCC::Equal, xf, yf);
        let xb = self.b.ins().band_imm_u(x.v, 0xff);
        let yb = self.b.ins().band_imm_u(y.v, 0xff);
        let bool_eq = self.b.ins().icmp(IntCC::Equal, xb, yb);
        let ptr_eq = self.b.ins().icmp(IntCC::Equal, x.v, y.v);
        let one = self.b.ins().iconst(types::I8, 1);
        let r = self.b.ins().select(is_ptr, ptr_eq, one);
        let r = self.b.ins().select(is_bool, bool_eq, r);
        let r = self.b.ins().select(is_num, num_eq, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(slow);
        let r = self.op(OP_STRICT_EQ, xt, x.v, Some(y));
        let r = self.b.ins().ireduce(types::I8, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// `a && b` / `a || b`, keeping the value (not just its truth), as a `to`.
    fn logical(&mut self, op: LogicalOp, a: &HExpr, b: &HExpr, to: JV) -> RV {
        let ja = jv(&a.ty).unwrap();
        let counted = to.jk().counted();
        let (keep, rb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        for ty in cl_tys(to.jk()) {
            self.b.append_block_param(done, ty);
        }
        let x = self.value_of(a);
        // Owned (not pending) while its truth decides.
        let x = if ja.jk().counted() { self.own(x) } else { x };
        let t = self.truth_rv(x, ja.jk());
        if op == LogicalOp::And {
            self.b.ins().brif(t, rb, &[], keep, &[]);
        } else {
            self.b.ins().brif(t, keep, &[], rb, &[]);
        }
        self.b.seal_block(keep);
        self.b.seal_block(rb);
        self.b.switch_to_block(keep);
        // The left side is the result.
        let c = if ja.jk().counted() {
            let x = self.fresh(x);
            let c = self.coerce(x, ja, to);
            if c.owned {
                self.unpend(c);
            }
            c
        } else {
            self.coerce(x, ja, to)
        };
        let w: Vec<_> = c.words().into_iter().map(Into::into).collect();
        self.b.ins().jump(done, &w);
        self.b.switch_to_block(rb);
        if ja.jk().counted() {
            self.dec_rv(x);
        }
        let y = self.value_as(b, to);
        let y = if counted { self.own(y) } else { y };
        let w: Vec<_> = y.words().into_iter().map(Into::into).collect();
        self.b.ins().jump(done, &w);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let r = RV::of_words(&self.b.block_params(done).to_vec());
        if counted { self.fresh(r) } else { r }
    }

    /// `c ? a : b`, as a `to`.
    fn cond_expr(&mut self, c: &HExpr, a: &HExpr, b: &HExpr, to: JV) -> RV {
        let counted = to.jk().counted();
        let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        for ty in cl_tys(to.jk()) {
            self.b.append_block_param(done, ty);
        }
        let cv = self.truthy(c);
        self.b.ins().brif(cv, tb, &[], eb, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(eb);
        for (blk, x) in [(tb, a), (eb, b)] {
            self.b.switch_to_block(blk);
            let r = self.value_as(x, to);
            let r = if counted { self.own(r) } else { r };
            let w: Vec<_> = r.words().into_iter().map(Into::into).collect();
            self.b.ins().jump(done, &w);
        }
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let r = RV::of_words(&self.b.block_params(done).to_vec());
        if counted { self.fresh(r) } else { r }
    }

    /// A reference or value expression, as `jv(&e.ty)` holds it.
    fn rexpr(&mut self, e: &HExpr) -> RV {
        let want = jv(&e.ty).unwrap();
        let borrowed = RV::new;
        match &e.kind {
            HK::Null => borrowed(self.b.ins().iconst(types::I64, 0)),
            HK::Undefined => borrowed(self.b.ins().iconst(types::I64, 1)),
            HK::Str(s) => {
                let [t, p] = constant(Value::str(&**s));
                let t = self.b.ins().iconst(types::I64, t as i64);
                let p = self.b.ins().iconst(types::I64, p as i64);
                RV::val(t, p)
            }
            HK::Read(p) => self.read(p, &e.ty),
            HK::Closure(g) => {
                let r = self.make_closure(*g);
                self.coerce(r, JV::Val, want)
            }
            HK::Assign(p, x) => self.assign(p, x, want),
            HK::Compound(p, HBin::Concat, x) => {
                let a = concat_assign(p, x, &e.ty).unwrap();
                self.rexpr(&a)
            }
            HK::Cast(x) => self.value_as(x, want),
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.value_as(xs.last().unwrap(), want)
            }
            HK::Call(c) => self.call(c, want),
            HK::New { class, args } => {
                let ctor = self.hir.classes[*class as usize].ctor;
                if !self.el.new_direct(self.fid, *class, args) {
                    let k = self.b.ins().iconst(types::I32, CALL_CONSTRUCT as i64);
                    let a = self.b.ins().iconst(types::I64, *class as i64);
                    let r = self.interp_call(k, a, None, None, args, Vec::new());
                    return self.coerce(r, JV::Val, want);
                }
                let cls = self.b.ins().iconst(types::I64, std::rc::Rc::as_ptr(&self.classes[*class as usize]) as i64);
                let call = self.b.ins().call(self.h.new, &[cls]);
                let obj = self.b.inst_results(call)[0];
                let res = self.fresh(RV::new(obj));
                let mut vals = vec![self.ctx, obj];
                let pjs = self.param_jvs(ctor, false);
                let held = self.args(args, &pjs, &mut vals, false);
                let fr = self.funcs[&ctor];
                // The constructor's result (undefined) is not used.
                self.b.ins().call(fr, &vals);
                self.check_status();
                for h in held {
                    self.release(h);
                }
                res
            }
            HK::NewBuiltin(n, args) => {
                let k = self.b.ins().iconst(types::I32, CALL_NATIVE as i64);
                let a = self.b.ins().iconst(types::I64, *n as u16 as i64);
                let r = self.interp_call(k, a, None, None, args, Vec::new());
                self.coerce(r, JV::Val, want)
            }
            HK::Array(elems) => {
                // Number storage as the interpreter chooses it (see codegen's `HK::Array`).
                let numbers = match &e.ty {
                    Type::Array(t) if t.kind() == Kind::Num => true,
                    Type::Array(t) if matches!(**t, Type::Any | Type::Unknown) => {
                        elems.iter().all(|x| matches!(x, HElem::Expr(x) if x.ty.kind() == Kind::Num || matches!(x.kind, HK::Cast(ref y) if y.ty.kind() == Kind::Num)))
                    }
                    _ => false,
                };
                let n = self.b.ins().iconst(types::I32, numbers as i64);
                let call = self.b.ins().call(self.h.arr_new, &[n]);
                let a = self.b.inst_results(call)[0];
                let res = self.fresh(RV::new(a));
                for x in elems {
                    let HElem::Expr(x) = x else { unreachable!() };
                    let (tag, payload) = self.owned_words(x);
                    self.b.ins().call(self.h.arr_push, &[a, tag, payload]);
                }
                res
            }
            HK::Logical(op, a, b) => self.logical(*op, a, b, want),
            HK::Cond(c, a, b) => self.cond_expr(c, a, b, want),
            HK::Binary(HBin::Concat, a, b) => {
                let x = self.value_as(a, JV::Val);
                let x = if !x.owned && self.el.effects(b, is_var_read(self.hir, a)) { self.hold(x) } else { x };
                let y = self.value_as(b, JV::Val);
                let p = self.op(OP_CONCAT, x.t.unwrap(), x.v, Some(y));
                let r = self.helper_val(p);
                self.release(x);
                self.release(y);
                self.coerce(r, JV::Val, want)
            }
            HK::Unary(op @ (HUnary::ToStr | HUnary::TypeOf), x) => {
                let x = self.value_as(x, JV::Val);
                let x = if x.owned || matches!(op, HUnary::TypeOf) { x } else { self.hold(x) };
                let code = if matches!(op, HUnary::ToStr) { OP_TO_STR } else { OP_TYPEOF };
                let p = self.op(code, x.t.unwrap(), x.v, None);
                let r = self.helper_val(p);
                self.release(x);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// Read a variable, field, element or property, as a `jv(ty)`
    /// (borrowed, unless it had to be taken).
    fn read(&mut self, p: &Place, ty: &Type) -> RV {
        let want = jv(ty).unwrap();
        let counted = want.jk().counted();
        match p {
            Place::Var(v) => {
                let r = self.read_var(*v);
                self.coerce(r, var_jv(self.hir, *v), want)
            }
            Place::StrLen { obj, .. } => {
                let o = self.value_as(obj, JV::Val);
                let bits = self.op(OP_STR_LEN, o.t.unwrap(), o.v, None);
                self.check_status();
                self.release(o);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), bits))
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, None, ERR_READ);
                let r = self.load_field(o.v, *slot, want);
                // A field of an object this code owns only temporarily (a
                // call's result) must be taken before the object goes.
                if o.owned && counted {
                    self.inc_rv(r);
                    let r = self.fresh(r);
                    self.release(o);
                    return r;
                }
                self.release(o);
                r
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index]);
                let i = self.expr(index);
                let r = self.arr_get(a.v, i, want);
                if a.owned && counted {
                    self.inc_rv(r);
                    let r = self.fresh(r);
                    self.release(a);
                    return r;
                }
                self.release(a);
                r
            }
            Place::ArrLen { obj, .. } => {
                let a = self.value_as(obj, JV::Arr);
                let v = self.arr_len(a.v);
                self.release(a);
                RV::new(v)
            }
            Place::Prop { obj, name, .. } => {
                // A getter may run: the object must stay alive.
                let o = self.value_as(obj, JV::Val);
                let o = if is_var_read(self.hir, obj) { o } else { self.hold(o) };
                let s = self.b.ins().iconst(types::I64, site(name) as *const _ as i64);
                let call = self.b.ins().call(self.h.get_prop, &[s, o.t.unwrap(), o.v]);
                let p = self.b.inst_results(call)[0];
                let r = self.helper_val(p);
                self.release(o);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!(),
        }
    }

    /// `p = x`: the stored value (borrowed from where it was stored), as a `want`.
    fn assign(&mut self, p: &Place, x: &HExpr, want: JV) -> RV {
        let xj = jv(&x.ty).unwrap();
        match p {
            Place::Var(v) if self.borrowed.contains(v) => {
                let vj = var_jv(self.hir, *v);
                let r = self.value_as(x, vj);
                debug_assert!(!r.owned);
                self.stats.borrowed += 1;
                self.set_var(*v, r);
                self.coerce(r, vj, want)
            }
            Place::Var(v) => {
                let vj = var_jv(self.hir, *v);
                let r = self.value_as(x, vj);
                let r = if vj.jk().counted() { self.own(r) } else { r };
                if self.in_mem(*v) {
                    self.mem_set(*v, r);
                } else {
                    self.set_var(*v, r);
                }
                self.coerce(RV { owned: false, ..r }, vj, want)
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, Some(x), ERR_WRITE);
                let r = self.value_of(x);
                let r = if xj.jk().counted() { self.own(r) } else { r };
                self.store_field(o.v, *slot, xj, r);
                self.release(o);
                self.coerce(RV { owned: false, ..r }, xj, want)
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index, x]);
                let i = self.expr(index);
                let r = self.value_of(x);
                let r = if xj.jk().counted() { self.own(r) } else { r };
                self.arr_set(a.v, i, xj, r);
                self.release(a);
                self.coerce(RV { owned: false, ..r }, xj, want)
            }
            Place::Prop { obj, name, .. } => {
                let o = self.value_as(obj, JV::Val);
                let o = if !o.owned && (!is_var_read(self.hir, obj) || self.el.effects(x, true)) { self.hold(o) } else { o };
                let r = self.value_as(x, JV::Val);
                // One count for the property, one for the result (a setter may run).
                let r = self.hold(r);
                self.inc_rv(r);
                let s = self.b.ins().iconst(types::I64, site(name) as *const _ as i64);
                self.b.ins().call(self.h.set_prop, &[s, o.t.unwrap(), o.v, r.t.unwrap(), r.v]);
                self.check_status();
                self.release(o);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!(),
        }
    }

    /// What a compiled function takes (`this` first, if it has one and `this` is wanted).
    fn param_jvs(&self, g: FuncId, with_this: bool) -> Vec<JV> {
        let hg = &self.hir.funcs[g as usize];
        let this = if with_this { hg.this_var } else { None };
        this.iter().chain(hg.params.iter().map(|p| &p.var)).map(|v| var_jv(self.hir, *v)).collect()
    }

    /// Evaluate call arguments (as `pjs`) into `vals`. A borrowed argument
    /// must stay alive through the call: one read from a variable does (the
    /// callee can't change this function's variables) unless a later
    /// argument assigns; any other borrowed argument is owned for the call.
    /// Returns the references to release after the call.
    fn args(&mut self, args: &[HElem], pjs: &[JV], vals: &mut Vec<CV>, pure: bool) -> Vec<RV> {
        let mut held = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            let r = self.value_as(x, pjs[i]);
            if pjs[i].jk().counted() {
                let r = if self.must_hold(&r, x, &args[i + 1..], pure) { self.hold(r) } else { r };
                vals.extend(r.words());
                held.push(r);
            } else {
                vals.push(r.v);
            }
        }
        held
    }

    /// Must a borrowed argument `x` (value `r`) be owned for the call? A
    /// variable's value stays alive unless a later argument assigns; any
    /// other borrowed value, unless the callee is pure and no later
    /// argument has effects. A number or boolean has no count.
    fn must_hold(&self, r: &RV, x: &HExpr, later: &[HElem], pure: bool) -> bool {
        if r.owned || !jk(&x.ty).unwrap().counted() {
            return false;
        }
        let later_effects = |assigns_only: bool| later.iter().any(|a| matches!(a, HElem::Expr(y) if self.el.effects(y, assigns_only)));
        if is_var_read(self.hir, x) {
            later_effects(true)
        } else {
            !pure || later_effects(false)
        }
    }

    /// A call, its result as a `want`.
    fn call(&mut self, c: &HCall, want: JV) -> RV {
        if let Some(g) = self.el.direct_target(c) {
            return self.call_compiled(c, g, want);
        }
        match &c.callee {
            Callee::Native(Native::ArrPush) if self.el.inline_push(self.fid, c, want.jk()) => {
                let (t, [HElem::Expr(x)]) = (c.this.as_ref().unwrap(), &c.args[..]) else { unreachable!() };
                let a = self.arr_base(t, &[x]);
                let (tag, payload) = self.owned_words(x);
                let call = self.b.ins().call(self.h.arr_push, &[a.v, tag, payload]);
                let n = self.b.inst_results(call)[0];
                self.check_status();
                self.release(a);
                RV::new(n)
            }
            Callee::Native(n) if self.el.inline_math(self.fid, c, *n, want.jk()) => RV::new(self.math(*n, c)),
            _ => {
                let r = self.call_interp(c);
                self.coerce(r, JV::Val, want)
            }
        }
    }

    /// A call to a compiled function.
    fn call_compiled(&mut self, c: &HCall, g: FuncId, want: JV) -> RV {
        let hg = &self.hir.funcs[g as usize];
        let pure = self.el.pure.contains(&g);
        let mut vals = vec![self.ctx];
        let mut held = Vec::new();
        if let Some(tv) = hg.this_var {
            // An instance's methods take an object; static methods, their class (a value).
            let tj = var_jv(self.hir, tv);
            match &c.this {
                Some(t) => {
                    let r = self.value_as(t, tj);
                    let r = if self.must_hold(&r, t, &c.args, pure) { self.hold(r) } else { r };
                    if matches!(c.callee, Callee::Method { .. }) && tj.jk() == JK::Ref {
                        self.check_obj(r.v, ERR_METHOD);
                    }
                    vals.extend(r.words());
                    held.push(r);
                }
                None => {
                    let z = self.zero_rv(tj.jk());
                    vals.extend(z.words());
                }
            }
        }
        let pjs = self.param_jvs(g, false);
        held.extend(self.args(&c.args, &pjs, &mut vals, pure));
        let fr = self.funcs[&g];
        let call = self.b.ins().call(fr, &vals);
        let res = RV::of_words(&self.b.inst_results(call).to_vec());
        let rj = ret_jv(hg).unwrap();
        let res = if rj.jk().counted() { self.fresh(res) } else { res };
        self.check_status();
        for h in held {
            self.release(h);
        }
        self.coerce(res, rj, want)
    }

    /// A call through the interpreter (`h_call`): an owned value.
    fn call_interp(&mut self, c: &HCall) -> RV {
        let mut held = Vec::new();
        // The receiver, then the function, then the arguments.
        let this = match &c.this {
            Some(t) => {
                let r = self.value_as(t, JV::Val);
                let r = if self.must_hold(&r, t, &c.args, false) || matches!(c.callee, Callee::Value(_)) { self.hold(r) } else { r };
                held.push(r);
                Some(r)
            }
            None => None,
        };
        let iconst32 = |s: &mut Self, x: u32| s.b.ins().iconst(types::I32, x as i64);
        let (kind, a, b) = match &c.callee {
            Callee::Direct { func, closure } if !self.hir.funcs[*func as usize].free.is_empty() => {
                let v = closure.unwrap();
                let r = self.read_var(v);
                let r = self.coerce(r, var_jv(self.hir, v), JV::Val);
                let r = if self.in_mem(v) || c.args.iter().any(|a| matches!(a, HElem::Expr(y) if self.el.effects(y, true))) { self.hold(r) } else { r };
                held.push(r);
                (iconst32(self, CALL_VALUE), r.t.unwrap(), Some(r.v))
            }
            Callee::Direct { func, .. } => (iconst32(self, CALL_FUNC), self.b.ins().iconst(types::I64, *func as i64), None),
            Callee::Method { vslot, .. } => (iconst32(self, CALL_VSLOT), self.b.ins().iconst(types::I64, *vslot as i64), None),
            Callee::Prop(name) => (iconst32(self, CALL_NAMED), self.b.ins().iconst(types::I64, site(name) as *const _ as i64), None),
            Callee::Native(n) => (iconst32(self, CALL_NATIVE), self.b.ins().iconst(types::I64, *n as u16 as i64), None),
            Callee::Value(f) => {
                let r = self.value_as(f, JV::Val);
                let r = if self.must_hold(&r, f, &c.args, false) { self.hold(r) } else { r };
                held.push(r);
                (iconst32(self, CALL_VALUE), r.t.unwrap(), Some(r.v))
            }
        };
        self.interp_call(kind, a, b, this, &c.args, held)
    }

    /// `h_call(kind, a, b, this, args…)`: an owned value. `held` are
    /// references to release after the call.
    fn interp_call(&mut self, kind: CV, a: CV, b: Option<CV>, this: Option<RV>, args: &[HElem], mut held: Vec<RV>) -> RV {
        use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
        let n = args.len();
        let argv = if n > 0 {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
            self.b.ins().stack_addr(types::I64, slot, 0)
        } else {
            self.b.ins().iconst(types::I64, 0)
        };
        for (i, x) in args.iter().enumerate() {
            let HElem::Expr(x) = x else { unreachable!() };
            let r = self.value_as(x, JV::Val);
            let r = if self.must_hold(&r, x, &args[i + 1..], false) { self.hold(r) } else { r };
            self.b.ins().store(mf(), r.t.unwrap(), argv, (16 * i) as i32);
            self.b.ins().store(mf(), r.v, argv, (16 * i + 8) as i32);
            held.push(r);
        }
        let (tt, tp) = match this {
            Some(r) => (r.t.unwrap(), r.v),
            None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
        };
        let b = b.unwrap_or_else(|| self.b.ins().iconst(types::I64, 0));
        let argc = self.b.ins().iconst(types::I32, n as i64);
        let call = self.b.ins().call(self.h.call, &[kind, a, b, tt, tp, argv, argc]);
        let p = self.b.inst_results(call)[0];
        let r = self.helper_val(p);
        for h in held {
            self.release(h);
        }
        r
    }

    fn math(&mut self, n: Native, c: &HCall) -> CV {
        let args: Vec<CV> = c.args.iter().map(|a| if let HElem::Expr(x) = a { self.expr(x) } else { unreachable!() }).collect();
        let ins = self.b.ins();
        match n {
            Native::MathFloor => ins.floor(args[0]),
            Native::MathCeil => ins.ceil(args[0]),
            Native::MathTrunc => ins.trunc(args[0]),
            Native::MathSqrt => ins.sqrt(args[0]),
            Native::MathAbs => ins.fabs(args[0]),
            _ => {
                let (h, i, rest) = if let Some(i) = MATH1.iter().position(|m| *m == n) {
                    (self.h.math1, i, &args[..1])
                } else {
                    (self.h.math2, MATH2.iter().position(|m| *m == n).unwrap(), &args[..2])
                };
                let idx = self.b.ins().iconst(types::I32, i as i64);
                let mut all = vec![idx];
                all.extend_from_slice(rest);
                let call = self.b.ins().call(h, &all);
                self.b.inst_results(call)[0]
            }
        }
    }
}

fn strip_cast(e: &HExpr) -> &HExpr {
    match &e.kind {
        HK::Cast(x) => strip_cast(x),
        _ => e,
    }
}

/// The `JK`s a compiled function takes: `this` first, if it has one.
fn param_jks(el: &Elig, hf: &HFunc) -> Vec<JK> {
    hf.this_var.iter().chain(hf.params.iter().map(|p| &p.var)).map(|v| el.vjk(*v).unwrap()).collect()
}

/// The machine words a compiled function takes (after the context).
fn param_tys(el: &Elig, hf: &HFunc) -> Vec<types::Type> {
    param_jks(el, hf).into_iter().flat_map(cl_tys).collect()
}

fn var_jv(hir: &HProgram, v: VarId) -> JV {
    jv(&hir.vars[v as usize].ty).unwrap()
}

/// Compile every eligible function and attach its entry point to `prog`.
/// Returns how many were compiled.
pub fn compile(hir: &HProgram, prog: &mut Program) -> usize {
    if !unsafe { ENABLED } {
        return 0;
    }
    let mut subclasses: Vec<Vec<u32>> = vec![Vec::new(); hir.classes.len()];
    for k in 0..hir.classes.len() as u32 {
        let mut c = Some(k);
        while let Some(x) = c {
            subclasses[x as usize].push(k);
            c = hir.classes[x as usize].parent;
        }
    }
    // Eligible functions: a fixpoint, since a call must go to a compiled function.
    let mut el = Elig { hir, set: (0..hir.funcs.len() as FuncId).collect(), pure: FxHashSet::default(), subclasses };
    loop {
        let keep: FxHashSet<FuncId> = el.set.iter().copied().filter(|&f| el.func_ok(f)).collect();
        if keep.len() == el.set.len() {
            break;
        }
        el.set = keep;
    }
    el.compute_pure();
    let debug = std::env::var_os("RTS_JIT_DEBUG").is_some();
    if el.set.is_empty() {
        return 0;
    }
    let lay = Layout::new();
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    let Ok(isa) = cranelift_native::builder().and_then(|b| b.finish(settings::Flags::new(flags)).map_err(|_| "isa")) else {
        return 0;
    };
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let helpers: [(&str, *const u8); 24] = [
        ("rts_mod", h_mod as *const u8),
        ("rts_pow", h_pow as *const u8),
        ("rts_int32", h_int32 as *const u8),
        ("rts_math1", h_math1 as *const u8),
        ("rts_math2", h_math2 as *const u8),
        ("rts_new", h_new as *const u8),
        ("rts_dec", h_dec as *const u8),
        ("rts_drop_val", h_drop_val as *const u8),
        ("rts_throw", h_throw as *const u8),
        ("rts_arr_get", h_arr_get as *const u8),
        ("rts_arr_set", h_arr_set as *const u8),
        ("rts_arr_len", h_arr_len as *const u8),
        ("rts_arr_push", h_arr_push as *const u8),
        ("rts_arr_new", h_arr_new as *const u8),
        ("rts_throw_site", h_throw_site as *const u8),
        ("rts_inc_val", h_inc_val as *const u8),
        ("rts_call", h_call as *const u8),
        ("rts_get_prop", h_get_prop as *const u8),
        ("rts_set_prop", h_set_prop as *const u8),
        ("rts_op", h_op as *const u8),
        ("rts_new_cell", h_new_cell as *const u8),
        ("rts_closure", h_closure as *const u8),
        ("rts_throw_value", h_throw_value as *const u8),
        ("rts_iter_array", h_iter_array as *const u8),
    ];
    for (n, p) in helpers {
        jb.symbol(n, p);
    }
    let mut m = JITModule::new(jb);
    let ptr = m.target_config().pointer_type();

    let sig = |m: &JITModule, params: &[types::Type], ret: Option<types::Type>| -> Signature {
        let mut s = m.make_signature();
        s.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        s.returns.extend(ret.map(AbiParam::new));
        s
    };
    let sig_n = |m: &JITModule, params: &[types::Type], rets: &[types::Type]| -> Signature {
        let mut s = m.make_signature();
        s.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        s.returns.extend(rets.iter().map(|t| AbiParam::new(*t)));
        s
    };
    let (f64t, i64t, i32t) = (types::F64, types::I64, types::I32);
    let decl = |m: &mut JITModule, name: &str, s: Signature| m.declare_function(name, Linkage::Import, &s).unwrap();
    let h_ids = [
        { let s = sig(&m, &[f64t, f64t], Some(f64t)); decl(&mut m, "rts_mod", s) },
        { let s = sig(&m, &[f64t, f64t], Some(f64t)); decl(&mut m, "rts_pow", s) },
        { let s = sig(&m, &[f64t], Some(i32t)); decl(&mut m, "rts_int32", s) },
        { let s = sig(&m, &[i32t, f64t], Some(f64t)); decl(&mut m, "rts_math1", s) },
        { let s = sig(&m, &[i32t, f64t, f64t], Some(f64t)); decl(&mut m, "rts_math2", s) },
        { let s = sig(&m, &[i64t], Some(i64t)); decl(&mut m, "rts_new", s) },
        { let s = sig(&m, &[i64t], None); decl(&mut m, "rts_dec", s) },
        { let s = sig(&m, &[i64t, i64t], None); decl(&mut m, "rts_drop_val", s) },
        { let s = sig(&m, &[i32t, i64t, i64t], None); decl(&mut m, "rts_throw", s) },
        { let s = sig(&m, &[i64t, f64t, i32t], Some(i64t)); decl(&mut m, "rts_arr_get", s) },
        { let s = sig(&m, &[i64t, f64t, i64t, i64t, i32t], None); decl(&mut m, "rts_arr_set", s) },
        { let s = sig(&m, &[i64t], Some(f64t)); decl(&mut m, "rts_arr_len", s) },
        { let s = sig(&m, &[i64t, i64t, i64t], Some(f64t)); decl(&mut m, "rts_arr_push", s) },
        { let s = sig(&m, &[i32t], Some(i64t)); decl(&mut m, "rts_arr_new", s) },
        { let s = sig(&m, &[i32t], None); decl(&mut m, "rts_throw_site", s) },
        { let s = sig(&m, &[i64t, i64t], None); decl(&mut m, "rts_inc_val", s) },
        { let s = sig(&m, &[i32t, i64t, i64t, i64t, i64t, i64t, i32t], Some(i64t)); decl(&mut m, "rts_call", s) },
        { let s = sig(&m, &[i64t, i64t, i64t], Some(i64t)); decl(&mut m, "rts_get_prop", s) },
        { let s = sig(&m, &[i64t, i64t, i64t, i64t, i64t], None); decl(&mut m, "rts_set_prop", s) },
        { let s = sig(&m, &[i32t, i64t, i64t, i64t, i64t], Some(i64t)); decl(&mut m, "rts_op", s) },
        { let s = sig(&m, &[], Some(i64t)); decl(&mut m, "rts_new_cell", s) },
        { let s = sig(&m, &[i32t, i64t, i32t], Some(i64t)); decl(&mut m, "rts_closure", s) },
        { let s = sig(&m, &[i64t, i64t], None); decl(&mut m, "rts_throw_value", s) },
        { let s = sig(&m, &[i64t, i64t], Some(i64t)); decl(&mut m, "rts_iter_array", s) },
    ];

    // Declare every compiled function (typed) and its entry trampoline.
    let mut ids: Vec<(FuncId, ClFuncId, ClFuncId)> = Vec::new();
    let mut fsigs: FxHashMap<FuncId, Signature> = FxHashMap::default();
    let mut order: Vec<FuncId> = el.set.iter().copied().collect();
    order.sort();
    for &f in &order {
        let hf = &hir.funcs[f as usize];
        let mut params = vec![ptr];
        // A closure's cells come first.
        if !hf.free.is_empty() {
            params.push(ptr);
        }
        params.extend(param_tys(&el, hf));
        let s = sig_n(&m, &params, &cl_tys(ret_jk(hf).unwrap()));
        let id = m.declare_function(&format!("f{f}"), Linkage::Local, &s).unwrap();
        let eid = m.declare_function(&format!("e{f}"), Linkage::Local, &sig(&m, &[ptr, ptr], Some(types::I64))).unwrap();
        fsigs.insert(f, s);
        ids.push((f, id, eid));
    }
    let id_of: FxHashMap<FuncId, ClFuncId> = ids.iter().map(|(f, id, _)| (*f, *id)).collect();
    // Each function's variables.
    let mut vars_of: FxHashMap<FuncId, Vec<VarId>> = FxHashMap::default();
    for (v, vi) in hir.vars.iter().enumerate() {
        if let Some(o) = vi.owner
            && el.set.contains(&o)
        {
            vars_of.entry(o).or_default().push(v as VarId);
        }
    }

    // Global variables' indices, as codegen assigns them.
    let mut globals: FxHashMap<VarId, u32> = FxHashMap::default();
    let (mut n_gs, mut n_gv) = (0u32, 0u32);
    for (i, v) in hir.vars.iter().enumerate() {
        if v.owner.is_none() {
            if v.kind().is_scalar() {
                globals.insert(i as VarId, n_gs);
                n_gs += v.kind().width() as u32;
            } else {
                globals.insert(i as VarId, n_gv);
                n_gv += 1;
            }
        }
    }
    let mut stats = RcStats::default();
    let mut ctx = m.make_context();
    let mut fbc = FunctionBuilderContext::new();
    for &(f, id, eid) in &ids {
        let hf = &hir.funcs[f as usize];
        let rk = ret_jk(hf).unwrap();
        // The function itself.
        ctx.func.signature = fsigs[&f].clone();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let mut funcs = FxHashMap::default();
            for (g, gid) in &id_of {
                funcs.insert(*g, m.declare_func_in_func(*gid, b.func));
            }
            let hr: Vec<FuncRef> = h_ids.iter().map(|i| m.declare_func_in_func(*i, b.func)).collect();
            let h = Helpers {
                modf: hr[0],
                pow: hr[1],
                int32: hr[2],
                math1: hr[3],
                math2: hr[4],
                new: hr[5],
                dec: hr[6],
                drop_val: hr[7],
                throw: hr[8],
                arr_get: hr[9],
                arr_set: hr[10],
                arr_len: hr[11],
                arr_push: hr[12],
                arr_new: hr[13],
                throw_site: hr[14],
                inc_val: hr[15],
                call: hr[16],
                get_prop: hr[17],
                set_prop: hr[18],
                op: hr[19],
                new_cell: hr[20],
                closure: hr[21],
                throw_value: hr[22],
                iter_array: hr[23],
            };
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let params: Vec<CV> = b.block_params(entry).to_vec();
            let c = params[0];
            // Depth check.
            let depth = b.ins().load(types::I32, mf(), c, 0);
            let d1 = b.ins().iadd_imm_s(depth, 1);
            b.ins().store(mf(), d1, c, 0);
            let over = b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, d1, MAX_DEPTH as i64);
            let (bail, body) = (b.create_block(), b.create_block());
            b.ins().brif(over, bail, &[], body, &[]);
            b.seal_block(bail);
            b.seal_block(body);
            b.switch_to_block(bail);
            let one = b.ins().iconst(types::I32, STATUS_OVERFLOW as i64);
            b.ins().store(mf(), one, c, 4);
            b.ins().store(mf(), depth, c, 0);
            let z = match rk {
                JK::Num => vec![b.ins().f64const(f64::NAN)],
                JK::Bool => vec![b.ins().iconst(types::I8, 0)],
                JK::Ref => vec![b.ins().iconst(types::I64, 1)],
                JK::Val => vec![b.ins().iconst(types::I64, lay.tag_undef), b.ins().iconst(types::I64, 0)],
            };
            b.ins().return_(&z);
            b.switch_to_block(body);
            let unwind = b.create_block();
            let mut lw = Lower {
                hir,
                el: &el,
                fid: f,
                rk,
                rjv: ret_jv(hf).unwrap(),
                b,
                vars: FxHashMap::default(),
                tvars: FxHashMap::default(),
                cells: FxHashMap::default(),
                declared: FxHashSet::default(),
                env: (!hf.free.is_empty()).then(|| params[1]),
                owned: vec![],
                borrowed: FxHashSet::default(),
                ctx: c,
                depth,
                unwind,
                pending: vec![],
                loops: vec![],
                iters: FxHashMap::default(),
                funcs,
                h,
                lay: &lay,
                classes: &prog.classes,
                globals: &globals,
                stats: &mut stats,
            };
            // Parameters (borrowed, unless assigned: then owned).
            let mut assigned = FxHashSet::default();
            assigned_vars(&hf.body, &mut assigned);
            let pvars: Vec<VarId> = hf.this_var.iter().chain(hf.params.iter().map(|p| &p.var)).copied().collect();
            let all: Vec<VarId> = pvars.iter().chain(vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[])).copied().collect();
            lw.borrowed = el.borrowed_vars(f, &all);
            // Captured variables: no cell yet (a block or `Init` makes it).
            for &v in vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[]) {
                if hir.vars[v as usize].captured {
                    let x = lw.b.declare_var(types::I64);
                    let none = lw.b.ins().iconst(types::I64, 1);
                    lw.b.def_var(x, none);
                    lw.cells.insert(v, x);
                }
            }
            let mut words = params[if lw.env.is_some() { 2 } else { 1 }..].iter().copied();
            for v in &pvars {
                let k = lw.el.vjk(*v).unwrap();
                let r = if k.wide() {
                    let t = words.next().unwrap();
                    RV::val(t, words.next().unwrap())
                } else {
                    RV::new(words.next().unwrap())
                };
                if lw.cells.contains_key(v) {
                    // A captured parameter moves to a cell, which owns a copy.
                    lw.new_cell(*v);
                    lw.declared.insert(*v);
                    if k.counted() {
                        lw.inc_rv(r);
                    }
                    lw.mem_set(*v, RV { owned: true, ..r });
                    continue;
                }
                lw.def_var(*v, r);
                if k.counted() && assigned.contains(v) && !lw.borrowed.contains(v) {
                    lw.inc_rv(r);
                    lw.owned.push(*v);
                }
            }
            // Locals: declared here, so every path sees them (undefined at first).
            for &v in vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !pvars.contains(&v) && !lw.cells.contains_key(&v) {
                    lw.var(v);
                    if lw.el.vjk(v).unwrap().counted() && !lw.borrowed.contains(&v) {
                        lw.owned.push(v);
                    }
                }
            }
            lw.declare_iters(&hf.body);
            lw.stmts(&hf.body);
            // Falling off the end returns undefined.
            lw.release_vars(None);
            let z = lw.zero_rv(rk);
            lw.ret(z);
            // Unwinding: release the variables, return anything.
            lw.b.switch_to_block(lw.unwind);
            lw.b.seal_block(lw.unwind);
            lw.release_vars(None);
            let z = lw.zero_rv(rk);
            lw.ret(z);
            let _ = lw.fid;
            lw.b.finalize(m.isa().frontend_config());
        }
        if let Err(e) = m.define_function(id, &mut ctx) {
            if debug {
                eprintln!("rts jit: {}: {e:?}", hf.name);
            }
            return 0;
        }
        m.clear_context(&mut ctx);

        // The entry trampoline: unpack argument bits, call, pack the result.
        ctx.func.signature = sig(&m, &[ptr, ptr], Some(types::I64));
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let fr = m.declare_func_in_func(id, b.func);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let (c, argv) = (b.block_params(blk)[0], b.block_params(blk)[1]);
            let mut args = vec![c];
            let mut i = 0;
            if !hf.free.is_empty() {
                args.push(b.ins().load(types::I64, mf(), argv, 0));
                i = 1;
            }
            for k in param_jks(&el, hf) {
                for _ in 0..cl_tys(k).len() {
                    let bits = b.ins().load(types::I64, mf(), argv, (i * 8) as i32);
                    i += 1;
                    args.push(match k {
                        JK::Num => b.ins().bitcast(types::F64, MemFlagsData::new(), bits),
                        JK::Bool => b.ins().ireduce(types::I8, bits),
                        JK::Ref | JK::Val => bits,
                    });
                }
            }
            let call = b.ins().call(fr, &args);
            let rs = b.inst_results(call).to_vec();
            let out = match rk {
                JK::Num => b.ins().bitcast(types::I64, MemFlagsData::new(), rs[0]),
                JK::Bool => b.ins().uextend(types::I64, rs[0]),
                JK::Ref => rs[0],
                JK::Val => {
                    b.ins().store(mf(), rs[0], c, CTX_RET_TAG);
                    rs[1]
                }
            };
            b.ins().return_(&[out]);
            b.finalize(m.isa().frontend_config());
        }
        m.define_function(eid, &mut ctx).unwrap();
        m.clear_context(&mut ctx);
    }
    if m.finalize_definitions().is_err() {
        return 0;
    }
    if debug {
        let mut names: Vec<String> = order
            .iter()
            .map(|f| {
                let hf = &hir.funcs[*f as usize];
                match hf.class {
                    Some(c) => format!("{}.{}", hir.classes[c as usize].name, if hf.is_ctor { "constructor" } else { &hf.name }),
                    None => hf.name.to_string(),
                }
            })
            .collect();
        names.sort();
        eprintln!("rts jit: {} functions compiled: {}", ids.len(), names.join(" "));
        eprintln!("rts jit: count updates emitted: {} increments, {} decrements; {} borrowed uses needed none", stats.incs, stats.decs, stats.borrowed);
    }
    for &(f, _, eid) in &ids {
        let p = m.get_finalized_function(eid);
        let hf = &hir.funcs[f as usize];
        if let Some(fc) = prog.funcs.get_mut(f as usize).and_then(std::rc::Rc::get_mut) {
            fc.jit = Some(Box::new(JitFn {
                entry: unsafe { std::mem::transmute::<*const u8, Entry>(p) },
                this: hf.this_var.map(|v| var_jv(hir, v)),
                env: !hf.free.is_empty(),
                params: hf.params.iter().map(|p| var_jv(hir, p.var)).collect(),
                ret: ret_jv(hf).unwrap(),
            }));
        }
    }
    // The code must outlive the program.
    std::mem::forget(m);
    ids.len()
}
