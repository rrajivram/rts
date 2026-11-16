//! Native compilation with Cranelift.
//!
//! Stage 1: numeric functions. Stage 2: class instances. A function is
//! compiled when its parameters, locals and result are numbers, booleans or
//! references to class instances (possibly `null`/`undefined`), and its body
//! uses only arithmetic, comparisons, control flow, `Math.*`, field reads and
//! writes, `new`, and calls to other compiled functions (methods included,
//! when the class hierarchy shows a single target). Everything else keeps
//! running in the interpreter, which calls compiled code directly (see
//! `Vm::call_native_code`).
//!
//! **Values.** A number is an `f64`, a boolean an `i8`, a reference an `i64`:
//! the object's box address, or 0 for `null` and 1 for `undefined`. A field
//! is read straight from the object's inline slots (`Object::field_offset`),
//! checking the slot's tag.
//!
//! **Reference counts.** Compiled code maintains counts itself, inline:
//! - Locals own a reference. Parameters (and `this`) are borrowed from the
//!   caller, unless the function assigns to them.
//! - An expression's result is either *owned* (a call's result, `new`) or
//!   *borrowed* (a variable or field read). A borrowed result used at once
//!   (a field read's base, a comparison, a truth test) costs no count update;
//!   one that must outlive other code (stored, returned, passed while later
//!   arguments run code) takes a reference first.
//! - Returning a local moves its reference to the caller.
//! - A *pure* function (one that writes no field and calls only pure
//!   functions) can't free anything reachable from its arguments while it
//!   runs. So its variables that only ever hold such values (`x = x.next`)
//!   take no counts at all, and neither do the arguments passed to it.
//!
//! These are the count-elision rules; `--profile` builds count every update
//! compiled code still makes (`reference-count updates`).
//!
//! **Errors.** Compiled code never catches. A type error (a field read on
//! `null`, …) or stack overflow sets `JitCtx::status`, releases the
//! function's locals and returns; every compiled caller returns in turn, and
//! the interpreter throws at the call site. Temporaries being evaluated at
//! that moment are not released (they leak).
//!
//! Number semantics are the interpreter's: `%`, `**`, ToInt32 and the `Math`
//! functions call the same Rust code.

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
use crate::runtime::{ClassRt, Object, Value};
use crate::types::{Kind, Type};

/// Recursion limit of compiled code (as the interpreter's).
const MAX_DEPTH: u32 = 10_000;

/// State shared by compiled code and the interpreter.
#[repr(C)]
pub struct JitCtx {
    pub depth: u32,
    /// Non-zero: unwinding to the interpreter (`STATUS_*`).
    pub status: u32,
}

pub const STATUS_OVERFLOW: u32 = 1;
pub const STATUS_TYPE_ERROR: u32 = 2;
/// `throw new C("…")`: the interpreter constructs the error (`take_throw`).
pub const STATUS_THROW: u32 = 3;

struct CtxCell(UnsafeCell<JitCtx>);
// SAFETY: the VM, and so all compiled code, runs on one thread.
unsafe impl Sync for CtxCell {}
static CTX: CtxCell = CtxCell(UnsafeCell::new(JitCtx { depth: 0, status: 0 }));

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
    /// A class instance, `null` (0) or `undefined` (1).
    Ref,
}

impl JK {
    pub fn kind(self) -> Kind {
        match self {
            JK::Num => Kind::Num,
            JK::Bool => Kind::Bool,
            JK::Ref => Kind::Val,
        }
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
}

impl JV {
    pub fn jk(self) -> JK {
        match self {
            JV::Num => JK::Num,
            JV::Bool => JK::Bool,
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
    /// Takes `this` as its first argument.
    pub this: bool,
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
    /// A result's bits as a value (taking over a reference's count).
    #[inline]
    pub fn ret_value(&self, r: u64) -> Value {
        match self.ret {
            JV::Num => Value::Num(f64::from_bits(r)),
            JV::Bool => Value::Bool(r != 0),
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

const ERR_READ: u32 = 0;
const ERR_WRITE: u32 = 1;
const ERR_METHOD: u32 = 2;
const ERR_FIELD_NUM: u32 = 3;
const ERR_FIELD_BOOL: u32 = 4;
const ERR_FIELD_REF: u32 = 5;

/// Start unwinding with a type error.
extern "C" fn h_throw(code: u32, a: u64, b: u64) {
    let nullish = |p: u64| if p == 0 { "null" } else { "undefined" };
    let msg = match code {
        ERR_READ => format!("Cannot read properties of {}", nullish(a)),
        ERR_WRITE => format!("Cannot set properties of {}", nullish(a)),
        ERR_METHOD => format!("cannot call a method on {}", nullish(a)),
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
        Type::Null | Type::Undefined | Type::Void => Some(JV::Nil),
        Type::Union(ts) => ts.iter().try_fold(JV::Nil, |acc, t| match (acc, ref_class(t)?) {
            (a, JV::Nil) => Some(a),
            (JV::Nil, b) => Some(b),
            (a, b) if a == b => Some(a),
            _ => None,
        }),
        _ => None,
    }
}

fn is_ref_type(t: &Type) -> bool {
    ref_class(t).is_some()
}

/// How compiled code would hold a value of type `t`, if it can.
fn jv(t: &Type) -> Option<JV> {
    match t.kind() {
        Kind::Num => Some(JV::Num),
        Kind::Bool => Some(JV::Bool),
        Kind::Big => None,
        Kind::Val => ref_class(t),
    }
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
        Some(Kind::Val) => hf.ret_ty.as_ref().and_then(ref_class),
        Some(Kind::Big) => None,
    }
}

/// A function's result, as compiled code returns it (`void` is `undefined`).
fn ret_jk(hf: &HFunc) -> Option<JK> {
    match hf.ret_kind {
        None => Some(JK::Ref),
        Some(Kind::Num) => Some(JK::Num),
        Some(Kind::Bool) => Some(JK::Bool),
        Some(Kind::Val) => hf.ret_ty.as_ref().filter(|t| is_ref_type(t)).map(|_| JK::Ref),
        Some(Kind::Big) => None,
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
            HK::New { .. } | HK::Array(_) => !assigns_only,
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
        ss.iter().all(|s| match s {
            HStmt::Expr(e) | HStmt::Return(Some(e)) | HStmt::Init(_, Some(e)) => self.pure_expr(e),
            HStmt::Block(b, _) => self.pure_stmts(b),
            HStmt::If(c, t, e) => self.pure_expr(c) && self.pure_stmts(t) && self.pure_stmts(e),
            HStmt::Loop { test, body, update, .. } => {
                test.as_ref().is_none_or(|t| self.pure_expr(t)) && update.as_ref().is_none_or(|u| self.pure_expr(u)) && self.pure_stmts(body)
            }
            HStmt::Labeled(_, s) => self.pure_stmts(std::slice::from_ref(&**s)),
            _ => true,
        })
    }

    fn pure_expr(&self, e: &HExpr) -> bool {
        let top = match &e.kind {
            HK::Assign(p, _) | HK::Compound(p, _, _) | HK::Update { place: p, .. } => matches!(**p, Place::Var(_)),
            HK::New { .. } | HK::Array(_) => false,
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
        let mut set: FxHashSet<VarId> = vars.iter().copied().filter(|v| self.vjk(*v) == Some(JK::Ref)).collect();
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
        if std::env::var("RTS_JIT_WHY").is_ok_and(|n| *n == *hf.name) {
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
        if hf.is_async || hf.is_generator || !hf.free.is_empty() || f == self.hir.main {
            return false;
        }
        if let Some(t) = hf.this_var
            && !(self.local(f, t) && self.vjk(t) == Some(JK::Ref))
        {
            return false;
        }
        for p in &hf.params {
            if p.default.is_some() || p.rest || !self.local(f, p.var) || self.vjk(p.var).map(JK::kind) != Some(p.arrival) {
                return false;
            }
        }
        // Falling off the end would return undefined, which the interpreter
        // reports as a type error for a number or boolean result.
        if rk != JK::Ref && !always_returns(&hf.body) {
            return false;
        }
        let ok = hf.body.iter().all(|s| self.stmt(f, s));
        if !ok && std::env::var("RTS_JIT_WHY").is_ok_and(|n| *n == *hf.name) {
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
                HStmt::Block(b, _) => self.why(f, b),
                HStmt::If(c, t, e) if self.expr(f, c).is_some() => {
                    self.why(f, t);
                    self.why(f, e);
                }
                HStmt::Loop { body, fresh, test, update, .. } => {
                    if !fresh.iter().all(|v| self.local(f, *v)) {
                        eprintln!("rts jit: not compiled: loop with per-iteration variables {fresh:?}");
                    } else if let Some(t) = test.as_ref().filter(|t| self.expr(f, t).is_none()) {
                        self.why_expr(f, t);
                    } else if let Some(u) = update.as_ref().filter(|u| self.expr(f, u).is_none()) {
                        self.why_expr(f, u);
                    } else {
                        self.why(f, body);
                    }
                }
                HStmt::Expr(e) | HStmt::Return(Some(e)) | HStmt::Init(_, Some(e)) => self.why_expr(f, e),
                _ => eprintln!("rts jit: not compiled: {s:?}"),
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
        eprintln!("rts jit: not compiled ({:?}): {:?}", e.ty, e.kind);
    }

    fn stmt(&self, f: FuncId, s: &HStmt) -> bool {
        match s {
            HStmt::Expr(e) => self.expr(f, e).is_some(),
            HStmt::Init(v, e) => self.local(f, *v) && e.as_ref().is_none_or(|e| self.expr(f, e).is_some() && self.expr(f, e) == self.vjk(*v)),
            HStmt::Block(ss, vars) => vars.iter().all(|v| self.local(f, *v)) && ss.iter().all(|s| self.stmt(f, s)),
            HStmt::If(c, t, e) => self.cond(f, c) && t.iter().chain(e.iter()).all(|s| self.stmt(f, s)),
            HStmt::Throw(e) => throw_site(e).is_some(),
            // Per-iteration copies of `let` variables only matter to closures,
            // and compiled functions' variables are never captured.
            HStmt::Loop { test, body, update, fresh, .. } => {
                fresh.iter().all(|v| self.local(f, *v))
                    && test.as_ref().is_none_or(|t| self.cond(f, t))
                    && update.as_ref().is_none_or(|u| self.expr(f, u).is_some())
                    && body.iter().all(|s| self.stmt(f, s))
            }
            HStmt::Break(_) | HStmt::Continue(_) => true,
            HStmt::Labeled(_, s) => self.stmt(f, s),
            HStmt::Return(None) => ret_jk(&self.hir.funcs[f as usize]) == Some(JK::Ref),
            HStmt::Return(Some(e)) => {
                let k = self.expr(f, e);
                k.is_some() && k == ret_jk(&self.hir.funcs[f as usize])
            }
            _ => false,
        }
    }

    /// The expression's `JK`, if compiled code can evaluate it.
    fn expr(&self, f: FuncId, e: &HExpr) -> Option<JK> {
        let k = jk(&e.ty)?;
        let is = |x: &HExpr, want: JK| self.expr(f, x) == Some(want);
        let ok = match &e.kind {
            HK::Num(_) => k == JK::Num,
            HK::Bool(_) => k == JK::Bool,
            HK::Null | HK::Undefined => k == JK::Ref,
            HK::Read(p) => match &**p {
                Place::ArrLen { obj, opt: false } => k == JK::Num && self.is_array(f, obj),
                p => self.place(f, p, k),
            },
            HK::Binary(HBin::Num(_), a, b) | HK::Binary(HBin::NumCmp(_), a, b) => is(a, JK::Num) && is(b, JK::Num),
            HK::Binary(HBin::BoolEq(_), a, b) => is(a, JK::Bool) && is(b, JK::Bool),
            HK::Binary(HBin::StrictEq(_) | HBin::LooseEq(_), a, b) => {
                k == JK::Bool && self.expr(f, a).is_some() && self.expr(f, a) == self.expr(f, b)
            }
            HK::Unary(HUnary::NegNum | HUnary::BitNotNum, x) => is(x, JK::Num),
            HK::Unary(HUnary::NotBool, x) => is(x, JK::Bool),
            HK::Unary(HUnary::Truthy, x) => k == JK::Bool && self.cond(f, x),
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => is(a, k) && is(b, k),
            HK::Cond(c, a, b) => self.cond(f, c) && is(a, k) && is(b, k),
            HK::Assign(p, x) => self.place(f, p, k) && is(x, k),
            HK::Compound(p, HBin::Num(_), x) => k == JK::Num && self.place(f, p, JK::Num) && is(x, JK::Num),
            HK::Update { place, .. } => k == JK::Num && self.place(f, place, JK::Num),
            HK::Cast(x) => is(x, k),
            HK::Seq(xs) => !xs.is_empty() && xs.iter().all(|x| self.expr(f, x).is_some()) && is(xs.last().unwrap(), k),
            HK::Call(c) => self.call(f, c, k),
            HK::New { class, args } => {
                let ctor = self.hir.classes[*class as usize].ctor;
                k == JK::Ref && self.set.contains(&ctor) && self.hir.funcs[ctor as usize].this_var.is_some() && self.args(f, ctor, args)
            }
            HK::Array(elems) => jv(&e.ty) == Some(JV::Arr) && elems.iter().all(|x| matches!(x, HElem::Expr(x) if self.expr(f, x).is_some())),
            _ => false,
        };
        ok.then_some(k)
    }

    /// An expression used only for its truth: `&&`/`||` may mix kinds.
    fn cond(&self, f: FuncId, e: &HExpr) -> bool {
        match &e.kind {
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => self.cond(f, a) && self.cond(f, b),
            HK::Unary(HUnary::Truthy, x) => self.cond(f, x),
            _ => self.expr(f, e).is_some(),
        }
    }

    /// A variable or field compiled code can read and write as `k`. A field
    /// holds any value, so its kind is the access's type; reads check the tag.
    fn place(&self, f: FuncId, p: &Place, k: JK) -> bool {
        match p {
            Place::Var(v) => self.local(f, *v) && self.vjk(*v) == Some(k),
            Place::Field { obj, opt: false, .. } => self.expr(f, obj) == Some(JK::Ref) && jv(&obj.ty) == Some(JV::Obj),
            Place::Index { obj, index, opt: false } => self.is_array(f, obj) && self.expr(f, index) == Some(JK::Num),
            _ => false,
        }
    }

    fn is_array(&self, f: FuncId, e: &HExpr) -> bool {
        jv(&e.ty) == Some(JV::Arr) && self.expr(f, e).is_some()
    }

    fn args(&self, f: FuncId, callee: FuncId, args: &[HElem]) -> bool {
        let hf = &self.hir.funcs[callee as usize];
        hf.params.len() == args.len()
            && hf.params.iter().zip(args).all(|(p, a)| match a {
                HElem::Expr(x) => self.expr(f, x).is_some() && self.expr(f, x) == self.vjk(p.var),
                HElem::Spread(_) => false,
            })
    }

    fn call(&self, f: FuncId, c: &HCall, k: JK) -> bool {
        if c.opt {
            return false;
        }
        let target = match &c.callee {
            // A function that captures nothing ignores its closure.
            Callee::Direct { func, closure } if closure.is_none() || self.hir.funcs[*func as usize].free.is_empty() => Some(*func),
            Callee::Method { class, vslot } if c.this.is_some() => self.devirt(*class, *vslot),
            Callee::Native(Native::ArrPush) => {
                return k == JK::Num
                    && c.this.as_ref().is_some_and(|t| self.is_array(f, t))
                    && matches!(&c.args[..], [HElem::Expr(x)] if self.expr(f, x).is_some());
            }
            Callee::Native(n) => {
                let args: Vec<&HExpr> = c.args.iter().filter_map(|a| if let HElem::Expr(x) = a { Some(x) } else { None }).collect();
                if c.this.is_some() || args.len() != c.args.len() || k != JK::Num || !args.iter().all(|a| self.expr(f, a) == Some(JK::Num)) {
                    return false;
                }
                let one = matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(n);
                return (one && args.len() == 1) || (MATH2.contains(n) && args.len() == 2);
            }
            _ => None,
        };
        let Some(g) = target.filter(|g| self.set.contains(g)) else { return false };
        let hg = &self.hir.funcs[g as usize];
        let this_ok = match (&c.this, hg.this_var) {
            (Some(t), Some(_)) => self.expr(f, t) == Some(JK::Ref),
            (None, _) => true,
            (Some(_), None) => false,
        };
        this_ok && ret_jk(hg) == Some(k) && self.args(f, g, &c.args)
    }
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
        HK::Call(c) => c.this.iter().chain(elems(&c.args)).collect(),
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
            HStmt::Expr(x) | HStmt::Return(Some(x)) => e(x, out),
            HStmt::Init(v, Some(x)) => {
                out.push((*v, x));
                e(x, out);
            }
            HStmt::Block(b, _) => collect_ref_assigns(b, out),
            HStmt::If(c, t, f) => {
                e(c, out);
                collect_ref_assigns(t, out);
                collect_ref_assigns(f, out);
            }
            HStmt::Loop { test, body, update, .. } => {
                if let Some(t) = test {
                    e(t, out);
                }
                if let Some(u) = update {
                    e(u, out);
                }
                collect_ref_assigns(body, out);
            }
            HStmt::Labeled(_, s) => collect_ref_assigns(std::slice::from_ref(&**s), out),
            _ => {}
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

/// A read of a variable (which only an assignment can change).
fn is_var_read(e: &HExpr) -> bool {
    match &e.kind {
        HK::Read(p) => matches!(**p, Place::Var(_)),
        HK::Cast(x) => is_var_read(x),
        HK::Null | HK::Undefined => true,
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
        match s {
            HStmt::Expr(x) | HStmt::Return(Some(x)) => e(x, out),
            HStmt::Init(_, Some(x)) => e(x, out),
            HStmt::Block(b, _) => assigned_vars(b, out),
            HStmt::If(c, t, f) => {
                e(c, out);
                assigned_vars(t, out);
                assigned_vars(f, out);
            }
            HStmt::Loop { test, body, update, .. } => {
                if let Some(t) = test {
                    e(t, out);
                }
                if let Some(u) = update {
                    e(u, out);
                }
                assigned_vars(body, out);
            }
            HStmt::Labeled(_, s) => assigned_vars(std::slice::from_ref(&**s), out),
            _ => {}
        }
    }
}

// ───────────── lowering ─────────────

fn cl_ty(k: JK) -> types::Type {
    match k {
        JK::Num => types::F64,
        JK::Bool => types::I8,
        JK::Ref => types::I64,
    }
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
}

/// Layout facts compiled code relies on.
struct Layout {
    tag_undef: i64,
    tag_null: i64,
    tag_bool: i64,
    tag_num: i64,
    tag_obj: i64,
    tag_arr: i64,
    /// Tags from here on (`BigInt`, …) may own a box.
    tag_first_gc: i64,
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
            tag_first_gc: t(Value::BigInt(Gc::new(0))),
        };
        assert!(l.tag_undef < l.tag_first_gc && l.tag_null < l.tag_first_gc && l.tag_bool < l.tag_first_gc && l.tag_num < l.tag_first_gc);
        l
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
    b: FunctionBuilder<'b>,
    vars: FxHashMap<VarId, Variable>,
    /// Reference variables holding a counted reference (locals, and assigned parameters).
    owned: Vec<VarId>,
    /// Reference variables that hold borrowed values (see `Elig::borrowed_vars`).
    borrowed: FxHashSet<VarId>,
    ctx: CV,
    depth: CV,
    unwind: Block,
    /// Owned temporaries not yet consumed: released if the code unwinds.
    pending: Vec<CV>,
    /// (label, break target, continue target).
    loops: Vec<(Option<Name>, Block, Option<Block>)>,
    funcs: FxHashMap<FuncId, FuncRef>,
    h: Helpers,
    lay: &'a Layout,
    classes: &'a [std::rc::Rc<ClassRt>],
    stats: &'a mut RcStats,
}

/// A reference value and whether this code owns a count on it.
#[derive(Clone, Copy)]
struct RV {
    v: CV,
    owned: bool,
}

fn mf() -> MemFlagsData {
    MemFlagsData::trusted()
}

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
        x
    }

    fn zero_of(&mut self, k: JK) -> CV {
        match k {
            JK::Num => self.b.ins().f64const(f64::NAN),
            JK::Bool => self.b.ins().iconst(types::I8, 0),
            JK::Ref => self.b.ins().iconst(types::I64, 1),
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

    /// Take a reference (`p` may be null or undefined).
    fn inc(&mut self, p: CV) {
        self.stats.incs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, p, 2);
        self.if_then(real, |s| {
            let rc = s.b.ins().load(types::I32, mf(), p, 0);
            let rc1 = s.b.ins().iadd_imm_s(rc, 1);
            s.b.ins().store(mf(), rc1, p, 0);
            s.count_rc();
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
    fn fresh(&mut self, v: CV) -> RV {
        self.pending.push(v);
        RV { v, owned: true }
    }

    fn unpend(&mut self, v: CV) {
        let i = self.pending.iter().rposition(|p| *p == v).expect("pending temporary");
        self.pending.remove(i);
    }

    /// An owned copy of `r`, handed to the caller (who stores it at once).
    fn own(&mut self, r: RV) -> CV {
        if r.owned {
            self.unpend(r.v);
        } else {
            self.inc(r.v);
        }
        r.v
    }

    /// `r`, owned while other code runs.
    fn hold(&mut self, r: RV) -> RV {
        if r.owned {
            return r;
        }
        self.inc(r.v);
        self.fresh(r.v)
    }

    /// Done with `r`.
    fn release(&mut self, r: RV) {
        if r.owned {
            self.unpend(r.v);
            self.dec(r.v);
        } else {
            self.stats.borrowed += 1;
        }
    }

    /// Leave for the unwind block, releasing the pending temporaries.
    fn bail(&mut self) {
        for p in self.pending.clone() {
            self.dec(p);
        }
        self.b.ins().jump(self.unwind, &[]);
    }

    /// Release every owned reference variable (except `keep`).
    fn release_vars(&mut self, keep: Option<VarId>) {
        for v in self.owned.clone() {
            if Some(v) != keep {
                let x = self.var(v);
                let p = self.b.use_var(x);
                self.dec(p);
            }
        }
    }

    fn ret(&mut self, v: CV) {
        self.b.ins().store(mf(), self.depth, self.ctx, 0);
        self.b.ins().return_(&[v]);
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
            HStmt::Init(v, e) => {
                let k = self.el.vjk(*v).unwrap();
                let val = match e {
                    Some(e) if self.borrowed.contains(v) => {
                        let r = self.rexpr(e);
                        debug_assert!(!r.owned);
                        self.stats.borrowed += 1;
                        r.v
                    }
                    Some(e) if k == JK::Ref => {
                        let r = self.rexpr(e);
                        self.own(r)
                    }
                    Some(e) => self.expr(e),
                    None => self.zero_of(k),
                };
                self.set_var(*v, val);
            }
            HStmt::Block(ss, _) => self.stmts(ss),
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
            HStmt::Loop { label, test, body, update, test_after, .. } => {
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
                self.loops.push((label.clone(), xb, Some(ub)));
                self.stmts(body);
                self.loops.pop();
                self.b.ins().jump(ub, &[]);
                self.b.switch_to_block(ub);
                self.b.seal_block(ub);
                if let Some(u) = update {
                    self.discard(u);
                }
                self.b.ins().jump(tb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(bb);
                self.b.seal_block(xb);
                self.b.switch_to_block(xb);
            }
            HStmt::Labeled(name, s) if !matches!(&**s, HStmt::Loop { .. }) => {
                let xb = self.b.create_block();
                self.loops.push((Some(name.clone()), xb, None));
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
                let target = self.loops.iter().rev().find(|(l, _, c)| match label {
                    Some(n) => l.as_ref() == Some(n),
                    None => c.is_some(),
                });
                let (_, xb, cb) = target.expect("break target").clone();
                let to = if is_break { xb } else { cb.expect("continue target") };
                self.b.ins().jump(to, &[]);
                self.dead_block();
            }
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
                let v = match e {
                    None => {
                        self.release_vars(None);
                        self.b.ins().iconst(types::I64, 1)
                    }
                    Some(e) if self.rk == JK::Ref => {
                        // Returning an owned local moves its reference out.
                        if let HK::Read(p) = &strip_cast(e).kind
                            && let Place::Var(v) = &**p
                            && self.owned.contains(v)
                        {
                            let x = self.var(*v);
                            let val = self.b.use_var(x);
                            self.release_vars(Some(*v));
                            self.stats.borrowed += 1;
                            val
                        } else {
                            let r = self.rexpr(e);
                            let val = self.own(r);
                            self.release_vars(None);
                            val
                        }
                    }
                    Some(e) => {
                        let val = self.expr(e);
                        self.release_vars(None);
                        val
                    }
                };
                self.ret(v);
                self.dead_block();
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// Evaluate for effect.
    fn discard(&mut self, e: &HExpr) {
        if jk(&e.ty) == Some(JK::Ref) {
            let r = self.rexpr(e);
            self.release(r);
        } else {
            self.expr(e);
        }
    }

    /// Store an owned value into a variable, releasing the old one.
    fn set_var(&mut self, v: VarId, val: CV) {
        let x = self.var(v);
        if self.el.vjk(v) == Some(JK::Ref) && !self.borrowed.contains(&v) {
            let old = self.b.use_var(x);
            self.b.def_var(x, val);
            self.dec(old);
        } else {
            self.b.def_var(x, val);
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
            JK::Ref => {
                let r = self.rexpr(e);
                let t = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, r.v, 2);
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
        }
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

    // ── fields ──

    /// The object of a field access, checked. Held across `rest` (code that
    /// runs before the access) it must be owned unless nothing in `rest`
    /// could free it.
    fn field_base(&mut self, obj: &HExpr, rest: Option<&HExpr>, code: u32) -> RV {
        let r = self.rexpr(obj);
        let r = match rest {
            Some(x) if !r.owned && (if is_var_read(obj) { self.el.effects(x, true) } else { self.el.effects(x, false) }) => self.hold(r),
            _ => r,
        };
        self.check_obj(r.v, code);
        r
    }

    /// A value's tag (i8) and payload (i64) words.
    fn value_words(&mut self, vj: JV, v: CV) -> (CV, CV) {
        match vj {
            JV::Num => (self.b.ins().iconst(types::I8, self.lay.tag_num), self.b.ins().bitcast(types::I64, MemFlagsData::new(), v)),
            JV::Bool => (self.b.ins().iconst(types::I8, self.lay.tag_bool), self.b.ins().uextend(types::I64, v)),
            _ => {
                let heap = if vj == JV::Arr { self.lay.tag_arr } else { self.lay.tag_obj };
                let obj = self.b.ins().iconst(types::I8, heap);
                let null = self.b.ins().iconst(types::I8, self.lay.tag_null);
                let undef = self.b.ins().iconst(types::I8, self.lay.tag_undef);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, v, 0);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, v, 1);
                let t = self.b.ins().select(is_undef, undef, obj);
                (self.b.ins().select(is_null, null, t), v)
            }
        }
    }

    /// Read field `slot` of the object `p` as `want`.
    fn load_field(&mut self, p: CV, slot: u32, want: JV) -> CV {
        let off = Object::field_offset(slot as usize) as i32;
        let tag = self.b.ins().load(types::I8, mf(), p, off);
        let tag = self.b.ins().uextend(types::I64, tag);
        let payload = self.b.ins().load(types::I64, mf(), p, off + 8);
        match want {
            JV::Num => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_num);
                self.throw_if(bad, ERR_FIELD_NUM, tag, payload);
                self.b.ins().bitcast(types::F64, MemFlagsData::new(), payload)
            }
            JV::Bool => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_bool);
                self.throw_if(bad, ERR_FIELD_BOOL, tag, payload);
                self.b.ins().load(types::I8, mf(), p, off + 8)
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
                self.throw_if(bad, ERR_FIELD_REF, tag, payload);
                let zero = self.b.ins().iconst(types::I64, 0);
                let one = self.b.ins().iconst(types::I64, 1);
                let nullish = self.b.ins().select(is_null, zero, one);
                self.b.ins().select(is_heap, payload, nullish)
            }
        }
    }

    /// Store `v` (a `vj`, owned if a reference) into field `slot` of `p`,
    /// then release the old value.
    fn store_field(&mut self, p: CV, slot: u32, vj: JV, v: CV) {
        let off = Object::field_offset(slot as usize) as i32;
        let old_tag = self.b.ins().load(types::I8, mf(), p, off);
        let old_tag = self.b.ins().uextend(types::I64, old_tag);
        let old = self.b.ins().load(types::I64, mf(), p, off + 8);
        let (tag, payload) = self.value_words(vj, v);
        self.b.ins().store(mf(), tag, p, off);
        self.b.ins().store(mf(), payload, p, off + 8);
        // The old value: nothing to do unless it may own a box.
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
        let r = self.rexpr(obj);
        let var = is_var_read(obj);
        if !r.owned && rest.iter().any(|x| self.el.effects(x, var)) { self.hold(r) } else { r }
    }

    /// Read `a[i]` as `want`.
    fn arr_get(&mut self, a: CV, i: CV, want: JV) -> CV {
        let w = match want {
            JV::Num => W_NUM,
            JV::Bool => W_BOOL,
            JV::Obj => W_OBJ,
            JV::Arr => W_ARR,
            JV::Nil => W_NIL,
        };
        let w = self.b.ins().iconst(types::I32, w as i64);
        let call = self.b.ins().call(self.h.arr_get, &[a, i, w]);
        let r = self.b.inst_results(call)[0];
        self.check_status();
        match want {
            JV::Num => self.b.ins().bitcast(types::F64, MemFlagsData::new(), r),
            JV::Bool => self.b.ins().ireduce(types::I8, r),
            _ => r,
        }
    }

    /// `a[i] = v` (`v` a `vj`, owned if a reference).
    fn arr_set(&mut self, a: CV, i: CV, vj: JV, v: CV) {
        let (tag, payload) = self.value_words(vj, v);
        let tag = self.b.ins().uextend(types::I64, tag);
        let num = self.b.ins().iconst(types::I32, (vj == JV::Num) as i64);
        self.b.ins().call(self.h.arr_set, &[a, i, tag, payload, num]);
        self.check_status();
    }

    /// An owned value of `x` (a reference's count taken), as words for a helper.
    fn owned_words(&mut self, x: &HExpr) -> (CV, CV) {
        let vj = jv(&x.ty).unwrap();
        let v = if vj.jk() == JK::Ref {
            let r = self.rexpr(x);
            self.own(r)
        } else {
            self.expr(x)
        };
        let (tag, payload) = self.value_words(vj, v);
        (self.b.ins().uextend(types::I64, tag), payload)
    }

    // ── expressions ──

    /// A number or boolean expression.
    fn expr(&mut self, e: &HExpr) -> CV {
        let k = jk(&e.ty).expect("checked by Elig");
        if k == JK::Ref {
            // Only reached for `Seq`/`Cast` plumbing; callers use `rexpr`.
            let r = self.rexpr(e);
            return self.own(r);
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
                match jk(&a.ty).unwrap() {
                    JK::Num => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().fcmp(if *eq { FloatCC::Equal } else { FloatCC::NotEqual }, x, y)
                    }
                    JK::Bool => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().icmp(if *eq { IntCC::Equal } else { IntCC::NotEqual }, x, y)
                    }
                    JK::Ref => {
                        // Only identity is compared: a borrowed left side
                        // must stay alive (and unchanged) while the right runs.
                        let x = self.rexpr(a);
                        let x = if !x.owned && self.el.effects(b, is_var_read(a)) { self.hold(x) } else { x };
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
                let v = self.expr(x);
                self.b.ins().bxor_imm_u(v, 1)
            }
            HK::Unary(HUnary::Truthy, x) => self.truthy(x),
            HK::Logical(op, a, b) => {
                // a && b / a || b, keeping the value (not just its truth).
                let (rb, done) = (self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, cl_ty(k));
                let x = self.expr(a);
                let t = self.truth(x, k);
                if *op == LogicalOp::And {
                    self.b.ins().brif(t, rb, &[], done, &[x.into()]);
                } else {
                    self.b.ins().brif(t, done, &[x.into()], rb, &[]);
                }
                self.b.seal_block(rb);
                self.b.switch_to_block(rb);
                let y = self.expr(b);
                self.b.ins().jump(done, &[y.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                self.b.block_params(done)[0]
            }
            HK::Cond(c, a, b) => {
                let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, cl_ty(k));
                let cv = self.truthy(c);
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(eb);
                self.b.switch_to_block(tb);
                let x = self.expr(a);
                self.b.ins().jump(done, &[x.into()]);
                self.b.switch_to_block(eb);
                let y = self.expr(b);
                self.b.ins().jump(done, &[y.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                self.b.block_params(done)[0]
            }
            HK::Assign(p, x) => self.assign(p, x, k).v,
            HK::Compound(p, HBin::Num(op), x) => match &**p {
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
                    let cur = self.load_field(o.v, *slot, JV::Num);
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.store_field(o.v, *slot, JV::Num, r);
                    self.release(o);
                    r
                }
                Place::Index { obj, index, .. } => {
                    let a = self.arr_base(obj, &[index, x]);
                    let i = self.expr(index);
                    let cur = self.arr_get(a.v, i, JV::Num);
                    let y = self.expr(x);
                    let r = self.num_op(*op, cur, y);
                    self.arr_set(a.v, i, JV::Num, r);
                    self.release(a);
                    r
                }
                _ => unreachable!(),
            },
            HK::Update { place, inc, prefix } => {
                let one = self.b.ins().f64const(1.0);
                let step = |s: &mut Self, cur: CV| if *inc { s.b.ins().fadd(cur, one) } else { s.b.ins().fsub(cur, one) };
                match &**place {
                    Place::Var(v) => {
                        let var = self.var(*v);
                        let cur = self.b.use_var(var);
                        let r = step(self, cur);
                        self.b.def_var(var, r);
                        if *prefix { r } else { cur }
                    }
                    Place::Field { obj, slot, .. } => {
                        let o = self.field_base(obj, None, ERR_READ);
                        let cur = self.load_field(o.v, *slot, JV::Num);
                        let r = step(self, cur);
                        self.store_field(o.v, *slot, JV::Num, r);
                        self.release(o);
                        if *prefix { r } else { cur }
                    }
                    Place::Index { obj, index, .. } => {
                        let a = self.arr_base(obj, &[index]);
                        let i = self.expr(index);
                        let cur = self.arr_get(a.v, i, JV::Num);
                        let r = step(self, cur);
                        self.arr_set(a.v, i, JV::Num, r);
                        self.release(a);
                        if *prefix { r } else { cur }
                    }
                    _ => unreachable!(),
                }
            }
            HK::Cast(x) => self.expr(x),
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.expr(xs.last().unwrap())
            }
            HK::Call(c) => self.call(c, k).v,
            _ => unreachable!("checked by Elig"),
        }
    }

    /// A reference expression.
    fn rexpr(&mut self, e: &HExpr) -> RV {
        let borrowed = |v| RV { v, owned: false };
        match &e.kind {
            HK::Null => borrowed(self.b.ins().iconst(types::I64, 0)),
            HK::Undefined => borrowed(self.b.ins().iconst(types::I64, 1)),
            HK::Read(p) => self.read(p, &e.ty),
            HK::Assign(p, x) => self.assign(p, x, JK::Ref),
            HK::Cast(x) => self.rexpr(x),
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.rexpr(xs.last().unwrap())
            }
            HK::Call(c) => self.call(c, JK::Ref),
            HK::New { class, args } => {
                let ctor = self.hir.classes[*class as usize].ctor;
                let cls = self.b.ins().iconst(types::I64, std::rc::Rc::as_ptr(&self.classes[*class as usize]) as i64);
                let call = self.b.ins().call(self.h.new, &[cls]);
                let obj = self.b.inst_results(call)[0];
                let res = self.fresh(obj);
                let mut vals = vec![self.ctx, obj];
                let held = self.args(args, &mut vals, false);
                let fr = self.funcs[&ctor];
                // The constructor's result (undefined) is not used.
                self.b.ins().call(fr, &vals);
                self.check_status();
                for h in held {
                    self.release(h);
                }
                res
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
                let res = self.fresh(a);
                for x in elems {
                    let HElem::Expr(x) = x else { unreachable!() };
                    let (tag, payload) = self.owned_words(x);
                    self.b.ins().call(self.h.arr_push, &[a, tag, payload]);
                }
                res
            }
            HK::Logical(op, a, b) => {
                let (rb, done) = (self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, types::I64);
                let x = self.rexpr(a);
                let x = self.own(x);
                let t = self.truth(x, JK::Ref);
                if *op == LogicalOp::And {
                    // The left side is the result only when falsy (null/undefined: no count).
                    self.b.ins().brif(t, rb, &[], done, &[x.into()]);
                } else {
                    self.b.ins().brif(t, done, &[x.into()], rb, &[]);
                }
                self.b.seal_block(rb);
                self.b.switch_to_block(rb);
                self.dec(x);
                let y = self.rexpr(b);
                let y = self.own(y);
                self.b.ins().jump(done, &[y.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                let v = self.b.block_params(done)[0];
                self.fresh(v)
            }
            HK::Cond(c, a, b) => {
                let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, types::I64);
                let cv = self.truthy(c);
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(eb);
                self.b.switch_to_block(tb);
                let x = self.rexpr(a);
                let x = self.own(x);
                self.b.ins().jump(done, &[x.into()]);
                self.b.switch_to_block(eb);
                let y = self.rexpr(b);
                let y = self.own(y);
                self.b.ins().jump(done, &[y.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                let v = self.b.block_params(done)[0];
                self.fresh(v)
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// Read a variable or field (borrowed, for a reference).
    fn read(&mut self, p: &Place, ty: &Type) -> RV {
        let want = jv(ty).unwrap();
        let k = want.jk();
        match p {
            Place::Var(v) => {
                let x = self.var(*v);
                RV { v: self.b.use_var(x), owned: false }
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, None, ERR_READ);
                let v = self.load_field(o.v, *slot, want);
                // A field of an object this code owns only temporarily (a
                // call's result) must be taken before the object goes.
                if o.owned && k == JK::Ref {
                    self.inc(v);
                    let r = self.fresh(v);
                    self.release(o);
                    return r;
                }
                self.release(o);
                RV { v, owned: false }
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index]);
                let i = self.expr(index);
                let v = self.arr_get(a.v, i, want);
                if a.owned && k == JK::Ref {
                    self.inc(v);
                    let r = self.fresh(v);
                    self.release(a);
                    return r;
                }
                self.release(a);
                RV { v, owned: false }
            }
            Place::ArrLen { obj, .. } => {
                let a = self.rexpr(obj);
                let call = self.b.ins().call(self.h.arr_len, &[a.v]);
                let v = self.b.inst_results(call)[0];
                self.check_status();
                self.release(a);
                RV { v, owned: false }
            }
            _ => unreachable!(),
        }
    }

    /// `p = x`: the stored value (borrowed from where it was stored).
    fn assign(&mut self, p: &Place, x: &HExpr, k: JK) -> RV {
        match p {
            Place::Var(v) if self.borrowed.contains(v) => {
                let r = self.rexpr(x);
                debug_assert!(!r.owned);
                self.stats.borrowed += 1;
                self.set_var(*v, r.v);
                r
            }
            Place::Var(v) => {
                let val = if k == JK::Ref {
                    let r = self.rexpr(x);
                    self.own(r)
                } else {
                    self.expr(x)
                };
                self.set_var(*v, val);
                RV { v: val, owned: false }
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, Some(x), ERR_WRITE);
                let val = if k == JK::Ref {
                    let r = self.rexpr(x);
                    self.own(r)
                } else {
                    self.expr(x)
                };
                self.store_field(o.v, *slot, jv(&x.ty).unwrap(), val);
                self.release(o);
                RV { v: val, owned: false }
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index, x]);
                let i = self.expr(index);
                let vj = jv(&x.ty).unwrap();
                let val = if k == JK::Ref {
                    let r = self.rexpr(x);
                    self.own(r)
                } else {
                    self.expr(x)
                };
                self.arr_set(a.v, i, vj, val);
                self.release(a);
                RV { v: val, owned: false }
            }
            _ => unreachable!(),
        }
    }

    /// Evaluate call arguments into `vals`. A borrowed argument must stay
    /// alive through the call: one read from a variable does (the callee
    /// can't change this function's variables) unless a later argument
    /// assigns; any other borrowed argument is owned for the call. Returns
    /// the references to release after the call.
    fn args(&mut self, args: &[HElem], vals: &mut Vec<CV>, pure: bool) -> Vec<RV> {
        let mut held = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            if jk(&x.ty) == Some(JK::Ref) {
                let r = self.rexpr(x);
                let r = if self.must_hold(&r, x, &args[i + 1..], pure) { self.hold(r) } else { r };
                vals.push(r.v);
                held.push(r);
            } else {
                vals.push(self.expr(x));
            }
        }
        held
    }

    /// Must a borrowed argument `x` (value `r`) be owned for the call? A
    /// variable's value stays alive unless a later argument assigns; any
    /// other borrowed value, unless the callee is pure and no later
    /// argument has effects.
    fn must_hold(&self, r: &RV, x: &HExpr, later: &[HElem], pure: bool) -> bool {
        if r.owned {
            return false;
        }
        let later_effects = |assigns_only: bool| later.iter().any(|a| matches!(a, HElem::Expr(y) if self.el.effects(y, assigns_only)));
        if is_var_read(x) {
            later_effects(true)
        } else {
            !pure || later_effects(false)
        }
    }

    fn call(&mut self, c: &HCall, k: JK) -> RV {
        let target = match &c.callee {
            Callee::Direct { func, .. } => *func,
            Callee::Method { class, vslot } => self.el.devirt(*class, *vslot).unwrap(),
            Callee::Native(Native::ArrPush) => {
                let (t, [HElem::Expr(x)]) = (c.this.as_ref().unwrap(), &c.args[..]) else { unreachable!() };
                let a = self.arr_base(t, &[x]);
                let (tag, payload) = self.owned_words(x);
                let call = self.b.ins().call(self.h.arr_push, &[a.v, tag, payload]);
                let n = self.b.inst_results(call)[0];
                self.check_status();
                self.release(a);
                return RV { v: n, owned: false };
            }
            Callee::Native(n) => return RV { v: self.math(*n, c), owned: false },
            _ => unreachable!("checked by Elig"),
        };
        let hg = &self.hir.funcs[target as usize];
        let pure = self.el.pure.contains(&target);
        let mut vals = vec![self.ctx];
        let mut held = Vec::new();
        if hg.this_var.is_some() {
            match &c.this {
                Some(t) => {
                    let r = self.rexpr(t);
                    let r = if self.must_hold(&r, t, &c.args, pure) { self.hold(r) } else { r };
                    if matches!(c.callee, Callee::Method { .. }) {
                        self.check_obj(r.v, ERR_METHOD);
                    }
                    vals.push(r.v);
                    held.push(r);
                }
                None => vals.push(self.b.ins().iconst(types::I64, 1)),
            }
        }
        held.extend(self.args(&c.args, &mut vals, pure));
        let fr = self.funcs[&target];
        let call = self.b.ins().call(fr, &vals);
        let r = self.b.inst_results(call)[0];
        let res = if k == JK::Ref { self.fresh(r) } else { RV { v: r, owned: false } };
        self.check_status();
        for h in held {
            self.release(h);
        }
        res
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
    let helpers: [(&str, *const u8); 15] = [
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
    ];

    // Declare every compiled function (typed) and its entry trampoline.
    let mut ids: Vec<(FuncId, ClFuncId, ClFuncId)> = Vec::new();
    let mut fsigs: FxHashMap<FuncId, Signature> = FxHashMap::default();
    let mut order: Vec<FuncId> = el.set.iter().copied().collect();
    order.sort();
    for &f in &order {
        let hf = &hir.funcs[f as usize];
        let mut params = vec![ptr];
        params.extend(param_jks(&el, hf).into_iter().map(cl_ty));
        let s = sig(&m, &params, Some(cl_ty(ret_jk(hf).unwrap())));
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
                JK::Num => b.ins().f64const(f64::NAN),
                JK::Bool => b.ins().iconst(types::I8, 0),
                JK::Ref => b.ins().iconst(types::I64, 1),
            };
            b.ins().return_(&[z]);
            b.switch_to_block(body);
            let unwind = b.create_block();
            let mut lw = Lower {
                hir,
                el: &el,
                fid: f,
                rk,
                b,
                vars: FxHashMap::default(),
                owned: vec![],
                borrowed: FxHashSet::default(),
                ctx: c,
                depth,
                unwind,
                pending: vec![],
                loops: vec![],
                funcs,
                h,
                lay: &lay,
                classes: &prog.classes,
                stats: &mut stats,
            };
            // Parameters (borrowed, unless assigned: then owned).
            let mut assigned = FxHashSet::default();
            assigned_vars(&hf.body, &mut assigned);
            let pvars: Vec<VarId> = hf.this_var.iter().chain(hf.params.iter().map(|p| &p.var)).copied().collect();
            let all: Vec<VarId> = pvars.iter().chain(vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[])).copied().collect();
            lw.borrowed = el.borrowed_vars(f, &all);
            for (v, x) in pvars.iter().zip(&params[1..]) {
                let var = lw.var(*v);
                lw.b.def_var(var, *x);
                if lw.el.vjk(*v) == Some(JK::Ref) && assigned.contains(v) && !lw.borrowed.contains(v) {
                    lw.inc(*x);
                    lw.owned.push(*v);
                }
            }
            // Locals: declared here, so every path sees them (undefined at first).
            for &v in vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !pvars.contains(&v) {
                    lw.var(v);
                    if lw.el.vjk(v) == Some(JK::Ref) && !lw.borrowed.contains(&v) {
                        lw.owned.push(v);
                    }
                }
            }
            lw.stmts(&hf.body);
            // Falling off the end returns undefined.
            lw.release_vars(None);
            let z = lw.zero_of(rk);
            lw.ret(z);
            // Unwinding: release the variables, return anything.
            lw.b.switch_to_block(lw.unwind);
            lw.b.seal_block(lw.unwind);
            lw.release_vars(None);
            let z = lw.zero_of(rk);
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
            for (i, k) in param_jks(&el, hf).into_iter().enumerate() {
                let bits = b.ins().load(types::I64, mf(), argv, (i * 8) as i32);
                args.push(match k {
                    JK::Num => b.ins().bitcast(types::F64, MemFlagsData::new(), bits),
                    JK::Bool => b.ins().ireduce(types::I8, bits),
                    JK::Ref => bits,
                });
            }
            let call = b.ins().call(fr, &args);
            let r = b.inst_results(call)[0];
            let out = match rk {
                JK::Num => b.ins().bitcast(types::I64, MemFlagsData::new(), r),
                JK::Bool => b.ins().uextend(types::I64, r),
                JK::Ref => r,
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
                this: hf.this_var.is_some(),
                params: hf.params.iter().map(|p| var_jv(hir, p.var)).collect(),
                ret: ret_jv(hf).unwrap(),
            }));
        }
    }
    // The code must outlive the program.
    std::mem::forget(m);
    ids.len()
}
