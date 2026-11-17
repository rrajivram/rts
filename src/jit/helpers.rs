//! Runtime functions compiled code calls (`h_*`), and compiled code's scope arenas.

use super::*;

// ───────────── helpers called by compiled code ─────────────

pub(super) extern "C" fn h_mod(a: f64, b: f64) -> f64 {
    natives::js_mod(a, b)
}
pub(super) extern "C" fn h_pow(a: f64, b: f64) -> f64 {
    natives::js_pow(a, b)
}
pub(super) extern "C" fn h_int32(x: f64) -> i32 {
    natives::to_int32(x)
}

pub(super) const MATH1: &[Native] = &[
    Native::MathRound, Native::MathSign, Native::MathSin, Native::MathCos, Native::MathTan, Native::MathAsin,
    Native::MathAcos, Native::MathAtan, Native::MathExp, Native::MathLog, Native::MathLog2, Native::MathLog10,
    Native::MathCbrt, Native::MathFround, Native::MathClz32,
];
pub(super) const MATH2: &[Native] = &[Native::MathAtan2, Native::MathPow, Native::MathMin, Native::MathMax, Native::MathHypot, Native::MathImul];

pub(super) extern "C" fn h_math1(i: u32, x: f64) -> f64 {
    natives::math1(MATH1[i as usize], x)
}
pub(super) extern "C" fn h_math2(i: u32, a: f64, b: f64) -> f64 {
    natives::math2(MATH2[i as usize], a, b)
}

/// `new C`: a blank instance, owned by the caller.
///
/// Also the safepoint of compiled code: the cycle collector may run here.
/// That is sound because compiled code holds no uncounted pointer across a
/// call (a borrowed reference is a variable's or a caller's, both counted).
/// The tracing GC can't run here: it would need compiled frames' roots.
pub(super) extern "C" fn h_new(class: *const ClassRt, arena: *const Arena) -> u64 {
    if !arena.is_null() {
        let c = unsafe {
            std::rc::Rc::increment_strong_count(class);
            std::rc::Rc::from_raw(class)
        };
        return Gc::into_header(Object::blank_in(c.shape.clone(), Some(c), unsafe { &*arena })).as_ptr() as u64;
    }
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
pub(super) extern "C" fn h_dec(p: u64) {
    heap::dec_header(unsafe { NonNull::new_unchecked(p as *mut Header) });
}

/// A value from its two words (tag word, payload), not owned.
pub(super) fn peek_value(tag: u64, payload: u64) -> std::mem::ManuallyDrop<Value> {
    std::mem::ManuallyDrop::new(unsafe { std::mem::transmute::<[u64; 2], Value>([tag & 0xff, payload]) })
}

/// Drop a value overwritten in a field.
pub(super) extern "C" fn h_drop_val(tag: u64, payload: u64) {
    drop(std::mem::ManuallyDrop::into_inner(peek_value(tag, payload)));
}

// ── arrays (the interpreter's semantics: see `ReadElement*`, `WriteElement*`) ──

/// What an element read wants (`h_arr_get`'s `want`).
pub(super) const W_NUM: u32 = 0;
pub(super) const W_BOOL: u32 = 1;
pub(super) const W_OBJ: u32 = 2;
pub(super) const W_ARR: u32 = 3;
pub(super) const W_NIL: u32 = 4;
/// Any value: the payload, with the tag in `JitCtx::ret_tag` (borrowed).
pub(super) const W_VAL: u32 = 5;

pub(super) fn type_error(msg: String) {
    ERROR.with(|e| *e.borrow_mut() = msg);
    unsafe { (*ctx()).status = STATUS_TYPE_ERROR };
}

/// An array's bits as a borrowed value (null and undefined too).
pub(super) fn peek_arr(a: u64) -> std::mem::ManuallyDrop<Value> {
    peek_ref(a, JV::Arr)
}

pub(super) fn valid_index(i: f64) -> Option<usize> {
    (i >= 0.0 && i.fract() == 0.0).then_some(i as usize)
}

/// `a[i]`, as `want`: the bits of a number or boolean, or a borrowed reference.
pub(super) extern "C" fn h_arr_get(a: u64, i: f64, want: u32) -> u64 {
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

pub(super) struct UndefCell(Value);
// SAFETY: `undefined` holds no pointer; it is only read.
unsafe impl Sync for UndefCell {}

/// `a[i] = v`, taking over `v`'s count.
pub(super) extern "C" fn h_arr_set(a: u64, i: f64, tag: u64, payload: u64, num: u32) {
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
pub(super) extern "C" fn h_arr_len(a: u64) -> f64 {
    match &*peek_arr(a) {
        Value::Arr(arr) => arr.len() as f64,
        other => {
            type_error(format!("Cannot read properties of {} (reading 'length')", other.to_display()));
            0.0
        }
    }
}

/// `a.push(v)`, taking over `v`'s count: the new length.
pub(super) extern "C" fn h_arr_push(a: u64, tag: u64, payload: u64) -> f64 {
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
pub(super) extern "C" fn h_arr_new(numbers: u32, arena: *const Arena) -> u64 {
    if !arena.is_null() {
        return Gc::into_header(crate::runtime::Array::new_in(numbers != 0, unsafe { &*arena })).as_ptr() as u64;
    }
    #[cfg(not(feature = "gc"))]
    if crate::runtime::cycles::wanted() {
        crate::runtime::cycles::collect();
    }
    let a = if numbers != 0 { crate::runtime::Array::new_num(Vec::new()) } else { crate::runtime::Array::new_val(Vec::new()) };
    Gc::into_header(a).as_ptr() as u64
}

// ── scope arenas (see `escape.rs`): compiled code's regions ──

/// `--no-arena` (and the tracing-GC build): allocate everything on the heap.
pub(super) static mut ARENAS: bool = !cfg!(feature = "gc");

/// `--no-arena`.
pub fn no_arenas() {
    unsafe { ARENAS = false };
}

pub(super) fn arenas_on() -> bool {
    unsafe { ARENAS }
}

/// Compiled code's active regions, innermost last (boxed: compiled code
/// holds their addresses), and arenas to reuse. The VM runs on one thread.
#[allow(clippy::vec_box)] // boxed: arenas must not move
pub(super) type Arenas = (Vec<Box<Arena>>, Vec<Box<Arena>>);
pub(super) struct RegionStack(UnsafeCell<Arenas>);
unsafe impl Sync for RegionStack {}
pub(super) static REGIONS: RegionStack = RegionStack(UnsafeCell::new((Vec::new(), Vec::new())));

#[allow(clippy::mut_from_ref)]
pub(super) fn regions() -> &'static mut Arenas {
    unsafe { &mut *REGIONS.0.get() }
}

/// How many regions compiled code has open (read inline on entry by
/// functions with regions, to unwind to on an error).
pub(super) static mut REGION_DEPTH: u64 = 0;

/// A block with scope-bound allocations starts: its arena's address.
pub(super) extern "C" fn h_region_enter() -> u64 {
    let (open, pool) = regions();
    let a = pool.pop().unwrap_or_default();
    let p = &*a as *const Arena as u64;
    open.push(a);
    unsafe { REGION_DEPTH += 1 };
    p
}

/// The innermost region's block ends: free everything in its arena.
pub(super) extern "C" fn h_region_exit() {
    let (open, pool) = regions();
    let a = open.pop().expect("region stack");
    unsafe { REGION_DEPTH -= 1 };
    a.reset();
    pool.push(a);
}

/// Unwinding: free the regions opened since the depth was `depth`.
pub(super) extern "C" fn h_region_unwind(depth: u64) {
    while unsafe { REGION_DEPTH } > depth {
        h_region_exit();
    }
}

/// Return a value's words from a helper: the payload, and its tag in
/// `JitCtx::ret_tag`. The count is not taken.
pub(super) fn give_borrowed(v: &Value) -> u64 {
    let [t, p] = JitFn::val_words(v);
    unsafe { (*ctx()).ret_tag = t };
    p
}

/// Return a value from a helper, handing its count to compiled code.
pub(super) fn give(v: Value) -> u64 {
    let p = give_borrowed(&v);
    std::mem::forget(v);
    p
}

/// Return a helper's result, or start unwinding with its exception
/// (returning `undefined`).
pub(super) fn give_result(r: Result<Value, Value>) -> u64 {
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
pub(super) extern "C" fn h_new_cell() -> u64 {
    Gc::into_header(Gc::new(RefCell::new(Value::Undef))).as_ptr() as u64
}

/// A closure of function `func` capturing the `n` cells at `cells`
/// (borrowed box addresses): its box address, counted.
pub(super) extern "C" fn h_closure(func: u32, cells: *const u64, n: u32) -> u64 {
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
pub(super) extern "C" fn h_throw_value(tag: u64, payload: u64) {
    raise(std::mem::ManuallyDrop::into_inner(peek_value(tag, payload)));
}

/// The array a `for-of` walks (counted): an array itself, or a new array
/// of another iterable's values. Errors (not iterable) set the status.
pub(super) extern "C" fn h_iter_array(tag: u64, payload: u64) -> u64 {
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

pub(super) extern "C" fn h_inc_val(tag: u64, payload: u64) {
    std::mem::forget((*peek_value(tag, payload)).clone());
}

thread_local! {
    /// Values compiled code uses as constants (string literals), kept alive
    /// for good, and its property sites. Roots of the tracing GC.
    pub(super) static CONSTS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    pub(super) static SITES: RefCell<Vec<&'static crate::bytecode::PropertySite>> = const { RefCell::new(Vec::new()) };
}

/// Trace the values compiled code holds for good (tracing-GC build).
#[cfg(feature = "gc")]
pub fn trace_roots(t: &mut heap::Tracer) {
    use heap::Trace;
    CONSTS.with(|c| c.borrow().iter().for_each(|v| v.trace(t)));
    SITES.with(|s| s.borrow().iter().for_each(|x| x.key.trace(t)));
}

/// A constant's words, the constant kept alive for good.
pub(super) fn constant(v: Value) -> [u64; 2] {
    let w = JitFn::val_words(&v);
    CONSTS.with(|c| c.borrow_mut().push(v));
    w
}

/// A property site of compiled code (its own lookup cache).
pub(super) fn site(name: &str) -> &'static crate::bytecode::PropertySite {
    let s: &'static _ = Box::leak(Box::new(crate::bytecode::PropertySite::new(name)));
    SITES.with(|x| x.borrow_mut().push(s));
    s
}

/// The interpreter, while compiled code runs.
pub(super) fn vm() -> &'static mut crate::vm::Vm {
    // SAFETY: set by `Vm::call_native_code` before entering compiled code,
    // which is only entered from there; the interpreter isn't otherwise
    // touched while compiled code runs.
    unsafe { &mut *(*ctx()).vm }
}

/// Run `f` (a call into the interpreter) with the depth of compiled code
/// counting toward the interpreter's, and restore it after.
pub(super) fn nested<T>(f: impl FnOnce(&mut crate::vm::Vm) -> T) -> T {
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
pub(super) const CALL_FUNC: u32 = 0;
pub(super) const CALL_VSLOT: u32 = 1;
pub(super) const CALL_NAMED: u32 = 2;
pub(super) const CALL_VALUE: u32 = 3;
pub(super) const CALL_NATIVE: u32 = 4;
pub(super) const CALL_CONSTRUCT: u32 = 5;

/// A call into the interpreter: `kind` says what `a` (and `b`) are; `this`
/// and the `argc` arguments at `argv` are borrowed values. The result is
/// owned (see `give`).
pub(super) extern "C" fn h_call(kind: u32, a: u64, b: u64, this_t: u64, this_p: u64, argv: *const Value, argc: u32) -> u64 {
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

/// A by-name method call site's cache in compiled code: the receiver's
/// shape and class (as their `Rc`s' bits) and the method's adapter. The
/// `Rc`s are kept, so a freed shape's address can't produce a false hit.
#[repr(C)]
#[derive(Default)]
pub(super) struct NamedCache {
    pub(super) shape: u64,
    pub(super) class: u64,
    pub(super) adapter: u64,
    keep: Option<(std::rc::Rc<crate::runtime::Shape>, std::rc::Rc<crate::runtime::ClassRt>)>,
}

/// `recv.name(…)` missing its cache (`NamedCache`): the call as `h_call`
/// makes it, after filling the cache when the name is a compiled method
/// of the receiver's class (with an adapter for this many arguments).
pub(super) extern "C" fn h_call_named(site: *const crate::bytecode::PropertySite, cache: *mut NamedCache, this_t: u64, this_p: u64, argv: *const Value, argc: u32) -> u64 {
    if unsafe { (*ctx()).depth } >= MAX_DEPTH {
        unsafe { (*ctx()).status = STATUS_OVERFLOW };
        return give(Value::Undef);
    }
    let site = unsafe { &*site };
    let args: &[Value] = if argc == 0 { &[] } else { unsafe { std::slice::from_raw_parts(argv, argc as usize) } };
    let this = (*peek_value(this_t, this_p)).clone();
    // A method of the receiver's class (not shadowed by an own property):
    // cache it if it has an adapter, and call it without a second lookup.
    if let Value::Obj(o) = &this
        && let Some(cls) = &o.class
        && o.lookup(&site.name).is_none()
    {
        let mut k = Some(cls.clone());
        let mut found = None;
        while let Some(c) = k {
            if let Some(fi) = c.methods.get(&*site.name) {
                found = Some(*fi);
                break;
            }
            k = c.parent.clone();
        }
        if let Some(fi) = found {
            let ad = super::lower::NativeEntry::method(fi, args.len());
            if ad != 0 {
                let shape = o.shape();
                let c = unsafe { &mut *cache };
                c.shape = unsafe { std::mem::transmute_copy::<_, u64>(&shape) };
                c.class = unsafe { std::mem::transmute_copy::<_, u64>(cls) };
                c.adapter = ad;
                c.keep = Some((shape, cls.clone()));
            }
            return give_result(nested(|vm| vm.jit_call_func(fi, this, args)));
        }
    }
    let r = nested(|vm| vm.jit_call_named(site, this, args));
    give_result(r)
}

/// `obj.name` (the site's), as an owned value.
pub(super) extern "C" fn h_get_prop(site: *const crate::bytecode::PropertySite, t: u64, p: u64) -> u64 {
    let r = nested(|vm| vm.jit_get_prop(unsafe { &*site }, &peek_value(t, p)));
    give_result(r)
}

/// `obj.name = v`, taking over `v`'s count.
pub(super) extern "C" fn h_set_prop(site: *const crate::bytecode::PropertySite, t: u64, p: u64, vt: u64, vp: u64) {
    let v = std::mem::ManuallyDrop::into_inner(peek_value(vt, vp));
    if let Err(e) = nested(|vm| vm.jit_set_prop(unsafe { &*site }, &peek_value(t, p), v)) {
        raise(e);
    }
}

/// Operations on values (`h_op`).
pub(super) const OP_STRICT_EQ: u32 = 0;
pub(super) const OP_LOOSE_EQ: u32 = 1;
pub(super) const OP_TRUTHY: u32 = 2;
pub(super) const OP_CONCAT: u32 = 3;
pub(super) const OP_TO_STR: u32 = 4;
pub(super) const OP_TYPEOF: u32 = 5;
/// `s.length`: a number's bits.
pub(super) const OP_STR_LEN: u32 = 6;
/// `OP_STR_CMP + CmpOp as u32`: string comparisons.
pub(super) const OP_STR_CMP: u32 = 8;

/// An operation on one or two borrowed values: a boolean (0/1), or an owned
/// value (see `give`).
pub(super) extern "C" fn h_op(op: u32, at: u64, ap: u64, bt: u64, bp: u64) -> u64 {
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

/// Untyped operations (`h_dyn`): the interpreter's `Dynamic*` instructions.
/// Owned value results.
pub(super) const DYN_ADD: u32 = 0;
pub(super) const DYN_NEG: u32 = 1;
pub(super) const DYN_BITNOT: u32 = 2;
/// `DYN_NUMERIC + NumOp`: a number, or a BigInt for two BigInts.
pub(super) const DYN_NUMERIC: u32 = 16;
/// Number results (f64 bits): `DYN_NUM + NumOp`, and ToNumber.
pub(super) const DYN_NUM: u32 = 32;
pub(super) const DYN_TO_NUM: u32 = 48;
/// Boolean results: `DYN_CMP + CmpOp`.
pub(super) const DYN_CMP: u32 = 64;

const NUM_OPS: [NumOp; 12] = [
    NumOp::Add, NumOp::Sub, NumOp::Mul, NumOp::Div, NumOp::Mod, NumOp::Exp,
    NumOp::BitAnd, NumOp::BitOr, NumOp::BitXor, NumOp::Shl, NumOp::Shr, NumOp::UShr,
];
const CMP_OPS: [CmpOp; 6] = [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne];

/// An untyped operation on borrowed values (`DYN_*` says which and what
/// it returns). Errors start unwinding.
pub(super) extern "C" fn h_dyn(op: u32, at: u64, ap: u64, bt: u64, bp: u64) -> u64 {
    let (a, b) = (peek_value(at, ap), peek_value(bt, bp));
    let num = |r: Result<f64, Value>| match r {
        Ok(x) => x.to_bits(),
        Err(e) => {
            raise(e);
            f64::NAN.to_bits()
        }
    };
    match op {
        DYN_ADD => give_result(nested(|vm| vm.dyn_add(&a, &b))),
        DYN_NEG => give_result(nested(|vm| vm.dyn_negate(&a))),
        DYN_BITNOT => give(crate::vm::dyn_bitnot(&a)),
        DYN_TO_NUM => num(match &*a {
            Value::BigInt(_) => Err(nested(|vm| vm.bigint_error(crate::runtime::bigint::TO_NUMBER))),
            v => Ok(v.to_number()),
        }),
        _ if op >= DYN_CMP => crate::vm::dyn_compare(CMP_OPS[(op - DYN_CMP) as usize], &a, &b) as u64,
        _ if op >= DYN_NUM => num(if matches!(*a, Value::BigInt(_)) || matches!(*b, Value::BigInt(_)) {
            Err(nested(|vm| vm.bigint_error(crate::runtime::bigint::TO_NUMBER)))
        } else {
            Ok(crate::vm::dyn_num(NUM_OPS[(op - DYN_NUM) as usize], &a, &b))
        }),
        _ => give_result(crate::vm::dyn_numeric(NUM_OPS[(op - DYN_NUMERIC) as usize], &a, &b).map_err(|e| nested(|vm| vm.bigint_error(e)))),
    }
}

/// `obj[key]` (both borrowed), as an owned value.
pub(super) extern "C" fn h_get_dyn(ot: u64, op: u64, kt: u64, kp: u64) -> u64 {
    give_result(nested(|vm| vm.read_dynamic(&peek_value(ot, op), &peek_value(kt, kp))))
}

/// `obj[key] = v`, taking over `v`'s count.
pub(super) extern "C" fn h_set_dyn(ot: u64, op: u64, kt: u64, kp: u64, vt: u64, vp: u64) {
    let v = std::mem::ManuallyDrop::into_inner(peek_value(vt, vp));
    if let Err(e) = nested(|vm| vm.write_dynamic(&peek_value(ot, op), &peek_value(kt, kp), v)) {
        raise(e);
    }
}

pub(super) const ERR_READ: u32 = 0;
pub(super) const ERR_WRITE: u32 = 1;
pub(super) const ERR_METHOD: u32 = 2;
pub(super) const ERR_FIELD_NUM: u32 = 3;
pub(super) const ERR_FIELD_BOOL: u32 = 4;
pub(super) const ERR_FIELD_REF: u32 = 5;
/// Unboxing a value (codegen's `coerce`).
pub(super) const ERR_UNBOX_NUM: u32 = 6;
pub(super) const ERR_UNBOX_BOOL: u32 = 7;
pub(super) const ERR_UNBOX_REF: u32 = 8;

/// Start unwinding with a type error.
pub(super) extern "C" fn h_throw(code: u32, a: u64, b: u64) {
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
