//! The interpreter. Frames index into two shared slot stacks (scalars and
//! values). Calls into JS functions don't recurse in Rust; only natives that
//! call back into JS (e.g. `arr.map(f)`) start a nested run loop.

use crate::runtime::Gc;
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use crate::bytecode::*;
use crate::hir::{CmpOp, NumOp};
use crate::runtime::natives::{self, as_index, call_native, get_dyn, js_mod, js_pow, keys_of, make_array, method_by_name, set_dyn, to_int32};
use crate::runtime::*;
use crate::types::Kind;

#[path = "vm_task.rs"]
mod task;
pub use task::{call_task_native, func_prototype, is_callable, iter_result, PROTOTYPE_KEY};
use task::Flow;

use crate::runtime::task::{CoKind, Coroutine};

const MAX_DEPTH: usize = 10_000;

/// Slot stacks are reserved once and never move, so a running frame's slot
/// pointers stay valid across calls. The memory is zeroed lazily by the OS:
/// only pages actually used become resident.
const SCALAR_STACK: usize = 1 << 22;
const VALUE_STACK: usize = 1 << 21;

/// A fixed-size slot stack of `n` zeroed slots, reserved straight from the
/// system allocator: the OS commits pages lazily, so this is address space,
/// not heap, and it stays out of the heap statistics.
pub struct Stack<T> {
    ptr: std::ptr::NonNull<T>,
    len: usize,
}

impl<T> Stack<T> {
    /// # Safety
    /// An all-zero `T` must be a valid value.
    unsafe fn zeroed(n: usize) -> Stack<T> {
        use std::alloc::GlobalAlloc;
        let layout = std::alloc::Layout::array::<T>(n).unwrap();
        let p = unsafe { std::alloc::System.alloc_zeroed(layout) } as *mut T;
        Stack { ptr: std::ptr::NonNull::new(p).expect("out of memory"), len: n }
    }
}

impl<T> std::ops::Deref for Stack<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> std::ops::DerefMut for Stack<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        use std::alloc::GlobalAlloc;
        unsafe {
            std::ptr::drop_in_place(&mut **self as *mut [T]);
            System.dealloc(self.ptr.as_ptr() as *mut u8, std::alloc::Layout::array::<T>(self.len).unwrap());
        }
    }
}

use std::alloc::System;

/// Where a returning function's result goes.
#[derive(Debug, Clone, Copy)]
enum Ret {
    /// Into the caller's slot (of this slot type, at this index).
    ToSlot(Kind, u16),
    /// Nowhere (the call's result is unused).
    Discard,
    /// Return to a native caller (`Vm::call`).
    Native,
}

/// A function of the running program. The program (held by the VM in an
/// `Rc`) outlives every frame, so frames borrow functions without refcounting.
#[derive(Debug, Clone, Copy)]
struct FnRef(*const FunctionCode);

impl Vm {
    /// A function's code. The program outlives every frame and is never
    /// changed, so the reference is not tied to a borrow of the VM (no `Rc`
    /// count traffic per call).
    #[inline(always)]
    fn code<'a>(&self, f: u32) -> &'a FunctionCode {
        unsafe { &*Rc::as_ptr(&self.prog.funcs[f as usize]) }
    }
}

impl FnRef {
    fn new(f: &Rc<FunctionCode>) -> FnRef {
        FnRef(Rc::as_ptr(f))
    }
    #[inline(always)]
    fn get<'a>(self) -> &'a FunctionCode {
        unsafe { &*self.0 }
    }
}

/// A call in progress.
#[derive(Debug)]
struct Frame {
    func: FnRef,
    /// The next instruction to run (saved while the frame is not on top).
    pc: usize,
    /// Where this frame's slots start in `Vm::scalars` and `Vm::values`.
    scalar_base: usize,
    value_base: usize,
    /// The closure being run, whose captured cells `ReadCapture` reads.
    closure: Option<Gc<Closure>>,
    ret: Ret,
}

pub struct Vm {
    pub prog: Rc<Program>,
    /// The number/boolean slots of every frame, one after another (each frame
    /// uses `scalar_slots` of them from its `scalar_base`).
    scalars: Stack<u64>,
    /// The value slots of every frame, likewise.
    values: Stack<Value>,
    /// Global variables: numbers/booleans and values.
    global_scalars: Vec<u64>,
    global_values: Vec<Value>,
    frames: Vec<Frame>,
    /// Active try handlers: (frame index, catch pc, exception slot).
    handlers: Vec<(usize, u32, ValSlot)>,
    out: std::io::BufWriter<std::io::Stdout>,
    pub start: Instant,
    native_ret: Value,
    /// Microtasks and timers.
    lp: task::EventLoop,
    /// `process.argv` after the executable: the script and its arguments.
    pub args: Vec<String>,
    /// The `process` object, created on first use.
    process: Option<Value>,
    /// Frames running a generator or async function (frame index and its
    /// coroutine), innermost last. Kept apart from `Frame` so ordinary calls
    /// don't pay for it.
    co_frames: Vec<(usize, Gc<Coroutine>)>,
    /// `console.group` indentation, `console.count` and `console.time` state.
    pub console_indent: usize,
    pub console_counts: rustc_hash::FxHashMap<String, u32>,
    pub console_timers: rustc_hash::FxHashMap<String, f64>,
    /// Native calls into the VM in progress: the tracing GC waits for them.
    gc_block: u32,
    /// Counted references behind borrowed slots of frames bound from values
    /// (see `lend_bound`): (frame index, value), released when it pops.
    lent: Vec<(u32, Value)>,
    /// Per-instruction profile (`--profile`).
    #[cfg(feature = "profile")]
    pub profile: Option<Box<crate::profile::Profile>>,
    /// Active scope regions, innermost last (see `escape.rs`).
    pub(crate) regions: Vec<task::RegionRec>,
    /// Arenas of exited regions, for reuse.
    /// Boxed: compiled code holds arenas' addresses (`call_native_code`).
    #[allow(clippy::vec_box)]
    pub(crate) arena_pool: Vec<Box<crate::runtime::arena::Arena>>,
    /// Frames given a region for their result by their caller: (frame
    /// index, index into `regions`), innermost last.
    pub(crate) result_regions: Vec<(u32, u32)>,
    /// Allocate scope-bound objects in arenas (off: `--no-arena`, and in the
    /// tracing-GC build).
    pub arenas: bool,
    /// Boxes rooted by hosts that run VM code while holding values
    /// (the event loop). Only the tracing GC reads these.
    host_roots: Vec<std::ptr::NonNull<crate::runtime::heap::Header>>,
}

/// A number slot's contents as an f64, and an f64 as number slot contents.
#[inline(always)]
fn from_bits(bits: u64) -> f64 {
    f64::from_bits(bits)
}
#[inline(always)]
fn bits(x: f64) -> u64 {
    x.to_bits()
}

/// The BigInt in scalar slots `at` (low half) and `at + 1` (high half).
#[inline(always)]
fn read_big(scalars: &[u64], at: usize) -> i128 {
    (((scalars[at + 1] as u128) << 64) | scalars[at] as u128) as i128
}
#[inline(always)]
fn write_big(scalars: &mut [u64], at: usize, n: i128) {
    scalars[at] = n as u64;
    scalars[at + 1] = ((n as u128) >> 64) as u64;
}

fn num_op(op: NumOp, x: f64, y: f64) -> f64 {
    match op {
        NumOp::Add => x + y,
        NumOp::Sub => x - y,
        NumOp::Mul => x * y,
        NumOp::Div => x / y,
        NumOp::Mod => js_mod(x, y),
        NumOp::Exp => js_pow(x, y),
        NumOp::BitAnd => (to_int32(x) & to_int32(y)) as f64,
        NumOp::BitOr => (to_int32(x) | to_int32(y)) as f64,
        NumOp::BitXor => (to_int32(x) ^ to_int32(y)) as f64,
        NumOp::Shl => to_int32(x).wrapping_shl(to_int32(y) as u32 & 31) as f64,
        NumOp::Shr => (to_int32(x) >> (to_int32(y) as u32 & 31)) as f64,
        NumOp::UShr => ((to_int32(x) as u32) >> (to_int32(y) as u32 & 31)) as f64,
    }
}

/// Arithmetic on two dynamic values (numbers first).
#[inline(always)]
fn dyn_num(op: NumOp, a: &Value, b: &Value) -> f64 {
    let (x, y) = match (a, b) {
        (Value::Num(x), Value::Num(y)) => (*x, *y),
        (a, b) => (a.to_number(), b.to_number()),
    };
    num_op(op, x, y)
}

/// `left op right` on two dynamic values, as a Value: numbers, or BigInts
/// when both are BigInts (a BigInt and anything else is a TypeError).
fn dyn_numeric(op: NumOp, a: &Value, b: &Value) -> Result<Value, bigint::BigIntError> {
    match (a, b) {
        (Value::BigInt(x), Value::BigInt(y)) => bigint::arithmetic(op, **x, **y).map(Value::bigint),
        (Value::BigInt(_), _) | (_, Value::BigInt(_)) => Err(bigint::MIX),
        (a, b) => Ok(Value::Num(dyn_num(op, a, b))),
    }
}

/// Whether `ordering` satisfies the relational operator `op`.
fn holds(op: CmpOp, o: std::cmp::Ordering) -> bool {
    match op {
        CmpOp::Lt => o.is_lt(),
        CmpOp::Le => o.is_le(),
        CmpOp::Gt => o.is_gt(),
        CmpOp::Ge => o.is_ge(),
        CmpOp::Eq => o.is_eq(),
        CmpOp::Ne => o.is_ne(),
    }
}

/// `left op right` (`<` etc.) on two dynamic values. BigInts compare exactly
/// with numbers and with numeric strings; NaN makes it false.
fn dyn_compare(op: CmpOp, a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => str_cmp(op, &x.s, &y.s),
        (Value::BigInt(x), Value::BigInt(y)) => bigint::compare(op, **x, **y),
        (Value::BigInt(x), Value::Str(s)) => bigint::parse(&s.s).is_some_and(|y| bigint::compare(op, **x, y)),
        (Value::Str(s), Value::BigInt(y)) => bigint::parse(&s.s).is_some_and(|x| bigint::compare(op, x, **y)),
        (Value::BigInt(x), b) => bigint::cmp_number(**x, b.to_number()).is_some_and(|o| holds(op, o)),
        (a, Value::BigInt(y)) => bigint::cmp_number(**y, a.to_number()).is_some_and(|o| holds(op, o.reverse())),
        (a, b) => cmp(op, a.to_number(), b.to_number()),
    }
}

#[inline(always)]
fn cmp(op: CmpOp, x: f64, y: f64) -> bool {
    match op {
        CmpOp::Lt => x < y,
        CmpOp::Le => x <= y,
        CmpOp::Gt => x > y,
        CmpOp::Ge => x >= y,
        CmpOp::Eq => x == y,
        CmpOp::Ne => x != y,
    }
}

pub(crate) fn str_cmp(op: CmpOp, a: &str, b: &str) -> bool {
    // JS compares UTF-16 code units; identical to byte order for ASCII.
    let o = if a.is_ascii() && b.is_ascii() {
        a.cmp(b)
    } else {
        a.encode_utf16().cmp(b.encode_utf16())
    };
    match op {
        CmpOp::Lt => o.is_lt(),
        CmpOp::Le => o.is_le(),
        CmpOp::Gt => o.is_gt(),
        CmpOp::Ge => o.is_ge(),
        CmpOp::Eq => o.is_eq(),
        CmpOp::Ne => o.is_ne(),
    }
}

pub(crate) fn describe(v: &Value) -> String {
    match v {
        Value::Undef => "undefined".into(),
        Value::Null => "null".into(),
        Value::Str(s) => format!("string \"{}\"", s.s),
        Value::Num(n) => format!("number {}", num_to_string(*n)),
        Value::Bool(b) => format!("boolean {b}"),
        v => v.type_of().into(),
    }
}

impl Vm {
    pub fn new(prog: Program) -> Vm {
        let prog = Rc::new(prog);
        Vm {
            // SAFETY: zero is 0 for u64 and `undefined` for Value.
            scalars: unsafe { Stack::zeroed(SCALAR_STACK) },
            values: unsafe { Stack::zeroed(VALUE_STACK) },
            global_scalars: vec![0; prog.global_scalars],
            global_values: vec![Value::Undef; prog.global_values],
            frames: Vec::new(),
            handlers: Vec::new(),
            out: std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()),
            start: Instant::now(),
            native_ret: Value::Undef,
            lp: task::EventLoop::default(),
            args: Vec::new(),
            process: None,
            co_frames: Vec::new(),
            gc_block: 0,
            lent: Vec::new(),
            #[cfg(feature = "profile")]
            profile: None,
            host_roots: Vec::new(),
            regions: Vec::new(),
            arena_pool: Vec::new(),
            result_regions: Vec::new(),
            arenas: !cfg!(feature = "gc"),
            console_indent: 0,
            console_counts: Default::default(),
            console_timers: Default::default(),
            prog,
        }
    }

    // ───────────── services for natives ─────────────

    pub fn print(&mut self, s: &str) {
        if self.console_indent > 0 {
            let pad = " ".repeat(self.console_indent);
            for line in s.split('\n') {
                let _ = writeln!(self.out, "{pad}{line}");
            }
            return;
        }
        let _ = writeln!(self.out, "{s}");
    }
    pub fn eprint(&mut self, s: &str) {
        let _ = self.out.flush();
        eprintln!("{s}");
    }
    pub fn flush(&mut self) {
        let _ = self.out.flush();
    }
    pub fn inspect(&self, v: &Value) -> String {
        self.inspect_depth(v, 2)
    }
    pub fn inspect_depth(&self, v: &Value, depth: usize) -> String {
        let prog = self.prog.clone();
        inspect_with_depth(v, &move |id| prog.funcs[id as usize].name.clone(), depth)
    }
    /// Write without a newline (`process.stdout.write`).
    pub fn write_out(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
    }
    pub fn make_error_pub(&mut self, msg: String) -> Value {
        self.make_error(self.prog.error_class, msg)
    }

    /// The `process` global.
    pub fn process_object(&mut self) -> Value {
        if let Some(p) = &self.process {
            return p.clone();
        }
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "rts".into());
        let mut argv = vec![Value::str(exe.clone())];
        argv.extend(self.args.iter().map(|a| Value::str(a.as_str())));
        let obj = |keys: &[&str], vals: Vec<Value>| Value::Obj(Object::new(Shape::new(keys.iter().map(|k| Rc::from(*k)).collect()), vals, None));
        let stdout = obj(&["write", "isTTY", "columns"], vec![Value::Native(Native::StdoutWrite), Value::Bool(false), Value::Num(80.0)]);
        let stderr = obj(&["write", "isTTY"], vec![Value::Native(Native::StderrWrite), Value::Bool(false)]);
        let env = call_native(self, Native::EnvObject, Value::Undef, vec![]).unwrap_or(Value::Undef);
        let versions = obj(&["node", "v8", "rts"], vec![Value::str("22.12.0"), Value::str("12.4.254.21-node.21"), Value::str(env!("CARGO_PKG_VERSION"))]);
        let hrtime = Value::Native(Native::Hrtime);
        let p = obj(
            &[
                "argv", "env", "platform", "arch", "pid", "version", "versions", "execPath", "exitCode", "cwd", "chdir", "exit",
                "nextTick", "stdout", "stderr", "hrtime", "on", "once", "off", "emitWarning",
            ],
            vec![
                Value::Arr(natives::make_array(argv)),
                env,
                Value::str(crate::node::natives::platform()),
                Value::str(crate::node::natives::arch()),
                Value::Num(std::process::id() as f64),
                Value::str("v22.12.0"),
                versions,
                Value::str(exe),
                Value::Undef,
                Value::Native(Native::ProcessCwd),
                Value::Native(Native::ProcessChdir),
                Value::Native(Native::ProcessExit),
                Value::Native(Native::NextTick),
                stdout,
                stderr,
                hrtime,
                Value::Native(Native::NoOp),
                Value::Native(Native::NoOp),
                Value::Native(Native::NoOp),
                Value::Native(Native::NoOp),
            ],
        );
        self.process = Some(p.clone());
        p
    }

    /// The exit status: `process.exitCode` if the program set it.
    pub fn exit_code(&self) -> i32 {
        match &self.process {
            Some(Value::Obj(o)) => match o.get("exitCode") {
                Some(Value::Num(n)) => n as i32,
                _ => 0,
            },
            _ => 0,
        }
    }

    fn make_error(&mut self, class: Option<u32>, msg: String) -> Value {
        let Some(cid) = class else {
            return Value::str(msg);
        };
        let class = self.prog.classes[cid as usize].clone();
        let n = class.shape.keys.len();
        let obj = Object::new(class.shape.clone(), vec![Value::Undef; n], Some(class.clone()));
        obj.set(&"message".into(), Value::str(msg));
        obj.set(&"name".into(), Value::Str(JsStr::new(&*class.name)));
        obj.set(&"stack".into(), Value::str(""));
        Value::Obj(obj)
    }
    pub fn type_error(&mut self, msg: String) -> Value {
        self.make_error(self.prog.type_error_class, msg)
    }
    pub fn range_error(&mut self, msg: String) -> Value {
        self.make_error(self.prog.range_error_class, msg)
    }
    pub fn bigint_error(&mut self, e: bigint::BigIntError) -> Value {
        match e {
            bigint::BigIntError::Range(m) => self.range_error(m.into()),
            bigint::BigIntError::Type(m) => self.type_error(m.into()),
        }
    }
    pub fn syntax_error(&mut self, msg: String) -> Value {
        self.make_error(self.prog.syntax_error_class.or(self.prog.error_class), msg)
    }

    // ───────────── entry points ─────────────

    /// Run the program's main function. Returns the uncaught exception, if any.
    pub fn run_main(&mut self) -> Result<(), Value> {
        crate::runtime::heap::start_run();
        let main = self.prog.funcs[self.prog.main as usize].clone();
        self.ensure(0, 0, &main)?;
        self.frames.push(Frame {
            func: FnRef::new(&main),
            pc: 0,
            scalar_base: 0,
            value_base: 0,
            closure: None,
            ret: Ret::Discard,
        });
        let r = self.run(0).and_then(|()| self.run_event_loop());
        self.flush();
        r
    }

    /// Call any callable value from native code. Native code may hold
    /// values in Rust locals, so the tracing GC waits until the call returns.
    pub fn call(&mut self, callee: &Value, this: Value, args: Vec<Value>) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = self.call_unblocked(callee, this, args);
        self.gc_block -= 1;
        r
    }

    fn call_unblocked(&mut self, callee: &Value, this: Value, args: Vec<Value>) -> Result<Value, Value> {
        match callee {
            Value::Native(n) => call_native(self, *n, this, args),
            Value::Bound(b) => {
                let mut all = b.args.clone();
                all.extend(args);
                self.call(&b.target.clone(), b.this.clone(), all)
            }
            _ => self.call_slice_unblocked(callee, this, &args),
        }
    }

    /// Call from native code with arguments borrowed from the caller (no allocation
    /// for the argument list — used by callbacks such as `map`'s).
    pub fn call_slice(&mut self, callee: &Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = self.call_slice_unblocked(callee, this, args);
        self.gc_block -= 1;
        r
    }

    fn call_slice_unblocked(&mut self, callee: &Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        if let Value::Func(c) = callee
            && self.code(c.func).jit.is_some()
            && let Some(r) = self.call_native_values(c.func, Some(c), &this, args)
        {
            return r;
        }
        let (func, closure) = match callee {
            Value::Func(c) => (self.prog.funcs[c.func as usize].clone(), Some(c.clone())),
            Value::Native(n) => return call_native(self, *n, this, args.to_vec()),
            Value::Bound(_) => return self.call(callee, this, args.to_vec()),
            v => return Err(self.type_error(format!("{} is not a function", describe(v)))),
        };
        if self.frames.len() >= MAX_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        let (sb, vb) = match self.frames.last() {
            Some(top) => (top.scalar_base + top.func.get().scalar_slots, top.value_base + top.func.get().value_slots),
            None => (0, 0),
        };
        self.ensure(sb, vb, &func)?;
        self.values[vb] = this;
        if func.rest {
            self.bind_values(&func, sb, vb, args.to_vec())?;
        } else {
            self.bind_slice(&func, sb, vb, args)?;
        }
        self.lend_bound(&func, vb, 0);
        let depth = self.frames.len();
        self.frames.push(Frame {
            func: FnRef::new(&func),
            pc: 0,
            scalar_base: sb,
            value_base: vb,
            closure,
            ret: Ret::Native,
        });
        self.safepoint();
        self.run(depth)?;
        Ok(std::mem::take(&mut self.native_ret))
    }

    // ── entry points for compiled code (`jit.rs`) ──
    //
    // Compiled code holds references the tracing GC can't see, so each of
    // these blocks it (as `call` does) while it runs.

    /// Call function `fid` (one that captures nothing), as `CallTarget::Direct`.
    pub(crate) fn jit_call_func(&mut self, fid: u32, this: Value, args: &[Value]) -> Result<Value, Value> {
        if let Some(r) = self.call_native_values(fid, None, &this, args) {
            return r;
        }
        let f = Value::Func(Gc::new(Closure { func: fid, caps: Box::new([]), props: Default::default() }));
        self.call_slice(&f, this, args)
    }

    /// `recv.<method in vtable slot>(…)`, as `CallTarget::Method`.
    pub(crate) fn jit_call_vslot(&mut self, slot: u32, recv: Value, args: &[Value]) -> Result<Value, Value> {
        match &recv {
            Value::Obj(o) if o.class.is_some() => {
                let fi = o.class.as_ref().unwrap().vtable[slot as usize];
                self.jit_call_func(fi, recv, args)
            }
            v => {
                let d = describe(v);
                Err(self.type_error(format!("cannot call a method on {d}")))
            }
        }
    }

    /// `recv.name(…)` with the method looked up by name, as `CallTarget::Property`.
    pub(crate) fn jit_call_named(&mut self, site: &PropertySite, recv: Value, args: &[Value]) -> Result<Value, Value> {
        if let Value::Obj(o) = &recv
            && let Some(cls) = &o.class
        {
            let hit = match &*site.cache.method.borrow() {
                Some((sh, c, fi)) if Rc::ptr_eq(c, cls) && o.has_shape(sh) => Some(*fi),
                _ => None,
            };
            let fi = match hit {
                Some(fi) => Some(fi),
                None if o.lookup(&site.name).is_none() => {
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
                        *site.cache.method.borrow_mut() = Some((o.shape(), cls.clone(), fi));
                    }
                    found
                }
                None => None,
            };
            if let Some(fi) = fi {
                return self.jit_call_func(fi, recv, args);
            }
        }
        self.gc_block += 1;
        let m = self.lookup_method(&recv, &site.name);
        self.gc_block -= 1;
        match m? {
            f @ (Value::Func(_) | Value::Native(_) | Value::Bound(_)) => self.call_slice(&f, recv, args),
            v => {
                let d = describe(&v);
                Err(self.type_error(format!("{}.{} is not a function (it is {d})", recv.type_of(), site.name)))
            }
        }
    }

    /// Any callable value, as `CallTarget::Value`.
    pub(crate) fn jit_call_value(&mut self, f: &Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        match f {
            Value::Class(c) => Err(self.type_error(format!("Class constructor {} cannot be invoked without 'new'", c.name))),
            Value::Func(c) if self.code(c.func).jit.is_some() => {
                let fid = c.func;
                match self.call_native_values(fid, Some(c), &this, args) {
                    Some(r) => r,
                    None => self.call_slice(f, this, args),
                }
            }
            Value::Func(_) | Value::Native(_) | Value::Bound(_) => self.call_slice(f, this, args),
            v => {
                let d = describe(v);
                Err(self.type_error(format!("{d} is not a function")))
            }
        }
    }

    /// The values `for-of` walks, as `ToIterationArray` makes them.
    pub(crate) fn jit_iteration_array(&mut self, v: &Value) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = natives::iterate(self, v).map(|xs| Value::Arr(Array::new_val(xs)));
        self.gc_block -= 1;
        r
    }

    /// A built-in, as `CallTarget::Native`.
    pub(crate) fn jit_call_native(&mut self, n: Native, this: Value, args: &[Value]) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = call_native(self, n, this, args.to_vec());
        self.gc_block -= 1;
        r
    }

    /// `recv.name`, as `ReadProperty`.
    pub(crate) fn jit_get_prop(&mut self, site: &PropertySite, recv: &Value) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = match recv {
            Value::Obj(ob) => {
                let hit = match &*site.cache.entry.borrow() {
                    Some((sh, slot)) if ob.has_shape(sh) => Some(ob.slot(*slot as usize)),
                    _ => None,
                };
                match hit {
                    Some(x) => self.read_accessor(x, recv),
                    None => self.get_prop_slow(recv, site),
                }
            }
            Value::Arr(a) if site.is_length => Ok(Value::Num(a.len() as f64)),
            _ => self.get_prop_slow(recv, site),
        };
        self.gc_block -= 1;
        r
    }

    /// `recv.name = val`, as `WriteProperty`.
    pub(crate) fn jit_set_prop(&mut self, site: &PropertySite, recv: &Value, val: Value) -> Result<(), Value> {
        self.gc_block += 1;
        let r = self.set_prop(recv, site, val);
        self.gc_block -= 1;
        r
    }

    /// `new C(…)` for a class whose constructor isn't compiled.
    pub(crate) fn jit_construct(&mut self, class: u32, args: &[Value]) -> Result<Value, Value> {
        let c = Value::Class(self.prog.classes[class as usize].clone());
        self.gc_block += 1;
        let r = self.construct(&c, args.to_vec());
        self.gc_block -= 1;
        r
    }

    /// `ToString` (template literals, `String(x)`).
    pub(crate) fn jit_to_string(&mut self, v: &Value) -> Result<Value, Value> {
        self.gc_block += 1;
        let r = match v {
            Value::Obj(_) => self.to_string_value(v).map(Value::Str),
            v => Ok(Value::Str(v.to_js_string())),
        };
        self.gc_block -= 1;
        r
    }

    /// Call a function compiled to native code (`jit.rs`), if this call site
    /// passes exactly its parameters, as values compiled code can take.
    /// None: use the interpreter.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn call_native_code(
        &mut self,
        jf: &crate::jit::JitFn,
        func: &FunctionCode,
        ci: &CallSite,
        closure: Option<&Gc<Closure>>,
        this: Option<&Value>,
        sb: usize,
        vb: usize,
    ) -> Option<Result<bool, Value>> {
        use crate::jit::{JitFn, JV};
        let n = func.params.len();
        if ci.has_spread || ci.args.len() != n || n > 15 || ci.args.iter().zip(&func.params).any(|(a, p)| a.0 != *p) {
            return None;
        }
        if let Some((k, _)) = ci.output
            && k != jf.ret.kind()
            && k != Kind::Val
        {
            return None;
        }
        // Two words per argument at most (a value: tag and payload). Only
        // the words written are read.
        let mut argv = [std::mem::MaybeUninit::<u64>::uninit(); 34];
        let argv_ptr = argv.as_mut_ptr() as *mut u64;
        let put = |i: usize, w: u64| unsafe { argv_ptr.add(i).write(w) };
        // The region this call site gives the callee's fresh result (0: the heap).
        let arena = if jf.rarena {
            let caller = self.frames.len() - 1;
            ci.region.and_then(|up| self.alloc_region(caller, up)).map_or(0, |r| &*self.regions[r].arena as *const _ as u64)
        } else {
            0
        };
        let mut i = jf.write_prefix(argv_ptr, closure, arena, this.unwrap_or(&Value::Undef))?;
        for (&(_, r), &k) in ci.args.iter().zip(&jf.params) {
            i = match k {
                JV::Num | JV::Bool => {
                    put(i, self.scalars[sb + r as usize]);
                    i + 1
                }
                // Borrowed: the caller's slot keeps it alive.
                _ => JitFn::write_arg(argv_ptr, i, k, &self.values[vb + r as usize])?,
            };
        }
        let r = match self.enter_native_code(jf, argv_ptr) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        match ci.output {
            Some((Kind::Val, d)) => self.values[vb + d as usize] = jf.ret_value(r),
            Some((_, d)) => self.scalars[sb + d as usize] = r,
            None => drop(jf.ret_value(r)),
        }
        // Compiled code pushed no frames: take the safepoint here, with
        // every value back in a slot.
        self.safepoint();
        Some(Ok(false))
    }

    /// Run compiled code with these argument words: its result bits, or
    /// the exception it unwound with.
    #[inline(always)]
    fn enter_native_code(&mut self, jf: &crate::jit::JitFn, argv: *const u64) -> Result<u64, Value> {
        let ctx = crate::jit::ctx();
        let r = unsafe {
            (*ctx).depth = (self.frames.len() as u32).max((*ctx).base_depth);
            // The VM doesn't move, nor do its global vectors grow, while it runs.
            if (*ctx).vm != self as *mut Vm || (*ctx).global_values != self.global_values.as_mut_ptr() {
                (*ctx).vm = self as *mut Vm;
                (*ctx).global_scalars = self.global_scalars.as_mut_ptr();
                (*ctx).global_values = self.global_values.as_mut_ptr();
            }
            (jf.entry)(ctx, argv)
        };
        let status = unsafe { (*ctx).status };
        if status != 0 {
            unsafe { (*ctx).status = 0 };
            return Err(if status == crate::jit::STATUS_EXCEPTION {
                crate::jit::take_exception()
            } else if status == crate::jit::STATUS_OVERFLOW {
                self.range_error("Maximum call stack size exceeded".into())
            } else if status == crate::jit::STATUS_THROW {
                let (cid, args) = crate::jit::take_throw();
                let c = Value::Class(self.prog.classes[cid as usize].clone());
                match self.construct(&c, args.iter().map(|s| Value::str(s.to_string())).collect()) {
                    Ok(e) | Err(e) => e,
                }
            } else {
                let m = crate::jit::take_error();
                self.type_error(m)
            });
        }
        Ok(r)
    }

    /// Call compiled function `fid` with values, if it is compiled and the
    /// arguments are values it takes. None: use the interpreter.
    fn call_native_values(&mut self, fid: u32, closure: Option<&Gc<Closure>>, this: &Value, args: &[Value]) -> Option<Result<Value, Value>> {
        use crate::jit::JitFn;
        let func = self.code(fid);
        let jf = func.jit.as_deref()?;
        // Extra arguments (a callback's index and array, say) are dropped.
        if args.len() < jf.params.len() || jf.params.len() > 15 {
            return None;
        }
        let args = &args[..jf.params.len()];
        let mut argv = [0u64; 35];
        let mut i = jf.write_prefix(argv.as_mut_ptr(), closure, 0, this)?;
        for (a, &k) in args.iter().zip(&jf.params) {
            i = JitFn::write_arg(argv.as_mut_ptr(), i, k, a)?;
        }
        Some(self.enter_native_code(jf, argv.as_ptr()).map(|r| jf.ret_value(r)))
    }

    /// Release a popped frame's slots: deterministic destruction. Borrowed
    /// slots were never counted, so they are cleared without a decrement.
    #[inline(always)]
    fn release_frame(&mut self, fr: &Frame) {
        let f = fr.func.get();
        let slots = &mut self.values[fr.value_base..fr.value_base + f.value_slots];
        let mut borrowed = f.borrowed;
        if borrowed == 0 {
            for x in slots {
                x.release();
            }
            return;
        }
        for (i, x) in slots.iter_mut().enumerate() {
            if borrowed & 1 != 0 {
                unsafe { std::ptr::write(x, Value::Undef) };
            } else {
                x.release();
            }
            borrowed >>= 1;
            if i >= 63 {
                borrowed = 0;
            }
        }
        if let Some(&(f, _)) = self.lent.last()
            && f as usize >= self.frames.len()
        {
            self.lent.retain(|(f, _)| (*f as usize) < self.frames.len());
        }
    }

    /// A frame was bound with counted values (a spread or rest call, or a
    /// call from native code): turn its borrowed slots into uncounted copies
    /// and keep the counted references in `lent` until the frame is popped.
    fn lend_bound(&mut self, func: &FunctionCode, vb: usize, from: usize) {
        let mut b = func.borrowed >> from;
        let mut i = from;
        let frame = self.frames.len() as u32;
        while b != 0 && i < 64 && i < func.value_slots {
            if b & 1 != 0 {
                let v = std::mem::take(&mut self.values[vb + i]);
                if !matches!(v, Value::Undef | Value::Null | Value::Bool(_) | Value::Num(_) | Value::Native(_)) {
                    unsafe { std::ptr::write(&mut self.values[vb + i], std::ptr::read(&v)) };
                    self.lent.push((frame, v));
                } else {
                    self.values[vb + i] = v;
                }
            }
            b >>= 1;
            i += 1;
        }
    }

    /// Check that a frame for `func` at these bases fits in the slot stacks.
    fn ensure(&mut self, sb: usize, vb: usize, func: &FunctionCode) -> Result<(), Value> {
        if sb + func.scalar_slots + 2 > self.scalars.len() || vb + func.value_slots + 1 > self.values.len() {
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        Ok(())
    }

    /// A safepoint, right after a frame was pushed: every live value is now
    /// in a VM slot or structure (the callee's `this`, arguments and closure
    /// included), and the interpreter holds no heap value borrowed.
    #[inline(always)]
    pub(crate) fn safepoint(&mut self) {
        #[cfg(not(feature = "gc"))]
        if crate::runtime::cycles::wanted() {
            Self::collect_cycles();
        }
        #[cfg(feature = "gc")]
        if crate::runtime::gc::wanted() && self.gc_block == 0 {
            self.collect_garbage();
        }
    }

    #[cfg(not(feature = "gc"))]
    #[cold]
    #[inline(never)]
    fn collect_cycles() {
        crate::runtime::cycles::collect();
    }

    /// Mark from every root and sweep (tracing-GC build).
    #[cfg(feature = "gc")]
    #[cold]
    #[inline(never)]
    pub fn collect_garbage(&mut self) {
        use crate::runtime::heap::{Trace, Tracer};
        let extent = self.frames.last().map_or(0, |f| f.value_base + f.func.get().value_slots);
        let this = &*self;
        crate::runtime::gc::collect(&mut |t: &mut Tracer| {
            for v in &this.values[..extent] {
                v.trace(t);
            }
            for v in &this.global_values {
                v.trace(t);
            }
            this.native_ret.trace(t);
            for f in &this.frames {
                if let Some(c) = &f.closure {
                    t.edge(c);
                }
            }
            for (_, c) in &this.co_frames {
                t.edge(c);
            }
            this.lp.trace(t);
            if let Some(p) = &this.process {
                p.trace(t);
            }
            for &h in &this.host_roots {
                (t.visit)(h);
            }
            for f in &this.prog.funcs {
                for v in &f.constants {
                    v.trace(t);
                }
                for s in &f.property_sites {
                    s.key.trace(t);
                }
            }
            for c in &this.prog.classes {
                Value::Class(c.clone()).trace(t);
            }
            for sh in &this.prog.shapes {
                crate::runtime::trace_shape(sh, t);
            }
            crate::jit::trace_roots(t);
        });
    }

    /// Call from the event loop or another host that has rooted what it
    /// holds (`host_roots`): collection stays allowed inside the call.
    pub(crate) fn call_hosted(&mut self, callee: &Value, this: Value, args: Vec<Value>) -> Result<Value, Value> {
        let mark = self.root(callee);
        self.root(&this);
        for a in &args {
            self.root(a);
        }
        let r = self.call_unblocked(callee, this, args);
        self.host_roots.truncate(mark);
        r
    }

    /// Root a value for the tracing GC (until `host_roots` is truncated).
    /// Returns the previous length, to truncate back to.
    pub(crate) fn root(&mut self, v: &Value) -> usize {
        let mark = self.host_roots.len();
        #[cfg(feature = "gc")]
        {
            use crate::runtime::heap::{Trace, Tracer};
            let roots = &mut self.host_roots;
            v.trace(&mut Tracer { visit: &mut |h| roots.push(h), shared: true });
        }
        #[cfg(not(feature = "gc"))]
        let _ = v;
        mark
    }

    /// Bind boxed argument values to a callee's parameter slots.
    fn bind_values(&mut self, func: &FunctionCode, sb: usize, vb: usize, mut args: Vec<Value>) -> Result<(), Value> {
        let np = func.params.len();
        if func.rest && np > 0 {
            let rest: Vec<Value> = if args.len() >= np { args.split_off(np - 1) } else { vec![] };
            args.truncate(np - 1);
            args.resize(np - 1, Value::Undef);
            args.push(Value::Arr(make_array(rest)));
        }
        self.bind_slice(func, sb, vb, &args)
    }

    /// Bind argument values (no rest packing) to a callee's parameter slots.
    fn bind_slice(&mut self, func: &FunctionCode, sb: usize, vb: usize, args: &[Value]) -> Result<(), Value> {
        let (mut si, mut vi) = (0usize, 1usize);
        for (i, k) in func.params.iter().enumerate() {
            let a = args.get(i);
            match k {
                Kind::Val => {
                    self.values[vb + vi] = a.cloned().unwrap_or(Value::Undef);
                    vi += 1;
                }
                k => {
                    match a {
                        Some(a) => self.unbox_into(*k, a, sb + si)?,
                        None => self.default_into(*k, sb + si),
                    }
                    si += k.width() as usize;
                }
            }
        }
        Ok(())
    }

    /// Store `v` (checked to be of kind `k`) into the scalar slot(s) at `at`.
    fn unbox_into(&mut self, k: Kind, v: &Value, at: usize) -> Result<(), Value> {
        match (k, v) {
            (Kind::Num, Value::Num(n)) => self.scalars[at] = bits(*n),
            (Kind::Bool, Value::Bool(x)) => self.scalars[at] = *x as u64,
            (Kind::Big, Value::BigInt(n)) => write_big(&mut self.scalars, at, **n),
            (Kind::Num, v) => return Err(self.type_error(format!("expected a number, got {}", describe(v)))),
            (Kind::Bool, v) => return Err(self.type_error(format!("expected a boolean, got {}", describe(v)))),
            (Kind::Big, v) => return Err(self.type_error(format!("expected a bigint, got {}", describe(v)))),
            (Kind::Val, _) => unreachable!(),
        }
        Ok(())
    }

    /// A missing argument for a scalar parameter.
    fn default_into(&mut self, k: Kind, at: usize) {
        match k {
            Kind::Num => self.scalars[at] = bits(f64::NAN),
            Kind::Big => write_big(&mut self.scalars, at, 0),
            _ => self.scalars[at] = 0,
        }
    }

    /// The scalar slot(s) at `at`, of kind `k`, as a Value.
    fn boxed_at(&self, k: Kind, at: usize) -> Value {
        match k {
            Kind::Num => Value::Num(from_bits(self.scalars[at])),
            Kind::Bool => Value::Bool(self.scalars[at] != 0),
            Kind::Big => Value::bigint(read_big(&self.scalars, at)),
            Kind::Val => unreachable!(),
        }
    }

    /// Read a caller's slot of the given kind as a Value.
    fn read_val(&self, k: Kind, r: u16, sb: usize, vb: usize) -> Value {
        match k {
            Kind::Val => self.values[vb + r as usize].clone(),
            k => self.boxed_at(k, sb + r as usize),
        }
    }

    // ───────────── calls ─────────────

    /// Perform a call. Returns Ok(true) if a JS frame was pushed.
    fn do_call(&mut self, caller: &FunctionCode, ci: &CallSite, sb: usize, vb: usize) -> Result<bool, Value> {
        // `this` is read without counting: the caller's slot keeps it alive
        // for the whole call. Paths that hand it on clone it (counted).
        let this: Option<std::mem::ManuallyDrop<Value>> =
            ci.this.map(|r| std::mem::ManuallyDrop::new(unsafe { std::ptr::read(&self.values[vb + r.index()]) }));
        let this_counted = |t: &Option<std::mem::ManuallyDrop<Value>>| t.as_deref().cloned().unwrap_or(Value::Undef);
        let (func, closure) = match &ci.target {
            CallTarget::Direct(fi) => (self.code(*fi), None),
            CallTarget::Value(r) => match &self.values[vb + r.index()] {
                Value::Func(c) => (self.code(c.func), Some(c.clone())),
                Value::Native(n) => {
                    let n = *n;
                    return self.call_native_ci(n, ci, this_counted(&this), sb, vb);
                }
                Value::Bound(b) => {
                    let b = b.clone();
                    let args = self.collect_args(ci, sb, vb)?;
                    let mut all = b.args.clone();
                    all.extend(args);
                    let r = self.call(&b.target, b.this.clone(), all)?;
                    if let Some((k, d)) = ci.output {
                        self.store(k, d, sb, vb, r)?;
                    }
                    return Ok(false);
                }
                Value::Class(c) => {
                    return Err(self.type_error(format!("Class constructor {} cannot be invoked without 'new'", c.name)));
                }
                v => {
                    let d = describe(v);
                    return Err(self.type_error(format!("{d} is not a function")));
                }
            },
            CallTarget::Method { slot, .. } => match this.as_deref() {
                Some(Value::Obj(o)) if o.class.is_some() => {
                    let fi = o.class.as_ref().unwrap().vtable[*slot as usize];
                    (self.code(fi), None)
                }
                Some(v) => {
                    let d = describe(v);
                    return Err(self.type_error(format!("cannot call a method on {d}")));
                }
                None => unreachable!(),
            },
            CallTarget::Property(p) => 'prop: {
                let site = &caller.property_sites[*p as usize];
                let recv = this_counted(&this);
                // Method inline cache: same shape and class as last time → same method.
                if let Value::Obj(o) = &recv
                    && let Some(cls) = &o.class
                {
                    let hit = match &*site.cache.method.borrow() {
                        Some((sh, c, fi)) if Rc::ptr_eq(c, cls) && o.has_shape(sh) => Some(*fi),
                        _ => None,
                    };
                    let fi = match hit {
                        Some(fi) => Some(fi),
                        None if o.lookup(&site.name).is_none() => {
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
                                *site.cache.method.borrow_mut() = Some((o.shape(), cls.clone(), fi));
                            }
                            found
                        }
                        None => None,
                    };
                    if let Some(fi) = fi {
                        break 'prop (self.code(fi), None);
                    }
                }
                match self.lookup_method(&recv, &site.name)? {
                    Value::Func(ref c) => (self.code(c.func), Some(c.clone())),
                    Value::Native(n) => return self.call_native_ci(n, ci, recv, sb, vb),
                    f @ Value::Bound(_) => {
                        let args = self.collect_args(ci, sb, vb)?;
                        let r = self.call(&f, recv, args)?;
                        if let Some((k, d)) = ci.output {
                            self.store(k, d, sb, vb, r)?;
                        }
                        return Ok(false);
                    }
                    v => {
                        let d = describe(&v);
                        return Err(self.type_error(format!("{}.{} is not a function (it is {d})", recv.type_of(), site.name)));
                    }
                }
            }
            CallTarget::Native(n) => {
                return self.call_native_ci(*n, ci, this_counted(&this), sb, vb);
            }
        };
        if let Some(jf) = &func.jit
            && let Some(r) = self.call_native_code(jf, func, ci, closure.as_ref(), this.as_deref(), sb, vb)
        {
            return r;
        }
        if self.frames.len() >= MAX_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        let nsb = sb + caller.scalar_slots;
        let nvb = vb + caller.value_slots;
        self.ensure(nsb, nvb, func)?;
        let borrowed = func.borrowed;
        match this {
            // Borrowed: the callee's copy is not counted.
            Some(t) if borrowed & 1 != 0 => unsafe { std::ptr::write(&mut self.values[nvb], std::mem::ManuallyDrop::into_inner(t)) },
            t => self.values[nvb] = this_counted(&t),
        }
        if ci.has_spread || (func.rest && !ci.packed_rest) {
            let args = self.collect_args(ci, sb, vb)?;
            self.bind_values(func, nsb, nvb, args)?;
            self.lend_bound(func, nvb, 1);
        } else {
            // Fast path: copy arguments slot to slot, converting kinds only on mismatch.
            let (mut si, mut vi) = (0usize, 1usize);
            for (i, pk) in func.params.iter().enumerate() {
                match ci.args.get(i) {
                    Some(&(ak, ar)) => {
                        if ak == *pk {
                            match ak {
                                Kind::Val if borrowed & (1 << vi) != 0 && vi < 64 => unsafe {
                                    // Borrowed: a bitwise copy, not counted.
                                    let v = std::ptr::read(&self.values[vb + ar as usize]);
                                    std::ptr::write(&mut self.values[nvb + vi], v);
                                },
                                Kind::Val => {
                                    self.values[nvb + vi] = self.values[vb + ar as usize].clone();
                                }
                                Kind::Big => {
                                    self.scalars[nsb + si] = self.scalars[sb + ar as usize];
                                    self.scalars[nsb + si + 1] = self.scalars[sb + ar as usize + 1];
                                }
                                _ => self.scalars[nsb + si] = self.scalars[sb + ar as usize],
                            }
                        } else if *pk == Kind::Val {
                            self.values[nvb + vi] = self.boxed_at(ak, sb + ar as usize);
                        } else {
                            let val = self.read_val(ak, ar, sb, vb);
                            self.unbox_into(*pk, &val, nsb + si)?;
                        }
                    }
                    None => match pk {
                        Kind::Val => self.values[nvb + vi] = Value::Undef,
                        k => self.default_into(*k, nsb + si),
                    },
                }
                if *pk == Kind::Val {
                    vi += 1;
                } else {
                    si += pk.width() as usize;
                }
            }
        }
        if let Some(up) = ci.region {
            self.give_result_region(up);
        }
        self.frames.push(Frame {
            func: FnRef(func as *const FunctionCode),
            pc: 0,
            scalar_base: nsb,
            value_base: nvb,
            closure,
            ret: match ci.output {
                Some((k, r)) => Ret::ToSlot(k, r),
                None => Ret::Discard,
            },
        });
        self.safepoint();
        Ok(true)
    }

    fn collect_args(&mut self, ci: &CallSite, sb: usize, vb: usize) -> Result<Vec<Value>, Value> {
        let mut out = Vec::with_capacity(ci.args.len());
        for (i, &(k, r)) in ci.args.iter().enumerate() {
            let v = self.read_val(k, r, sb, vb);
            if ci.spread.get(i).copied().unwrap_or(false) {
                out.extend(natives::iterate(self, &v)?);
            } else {
                out.push(v);
            }
        }
        Ok(out)
    }

    fn call_native_ci(&mut self, n: Native, ci: &CallSite, this: Value, sb: usize, vb: usize) -> Result<bool, Value> {
        let args = self.collect_args(ci, sb, vb)?;
        let r = call_native(self, n, this, args)?;
        if let Some((k, d)) = ci.output {
            self.store(k, d, sb, vb, r)?;
        }
        Ok(false)
    }

    fn store(&mut self, k: Kind, d: u16, sb: usize, vb: usize, val: Value) -> Result<(), Value> {
        match k {
            Kind::Val => self.values[vb + d as usize] = val,
            k => self.unbox_into(k, &val, sb + d as usize)?,
        }
        Ok(())
    }

    /// Method lookup by name for dynamic calls.
    #[inline(never)]
    fn lookup_method(&mut self, recv: &Value, name: &str) -> Result<Value, Value> {
        match recv {
            Value::Obj(o) => {
                if let Some(v) = o.get_chain(name) {
                    return Ok(v);
                }
                if let Some(c) = &o.class {
                    let mut k = Some(c.clone());
                    while let Some(c) = k {
                        if let Some(fi) = c.methods.get(name) {
                            return Ok(Value::Func(Gc::new(Closure {
                                func: *fi,
                                caps: Box::new([]),
                                props: Default::default(),
                            })));
                        }
                        k = c.parent.clone();
                    }
                }
                Ok(method_by_name(recv, name).map_or(Value::Undef, Value::Native))
            }
            Value::Undef | Value::Null => Err(self.type_error(format!(
                "Cannot read properties of {} (reading '{name}')",
                recv.to_display()
            ))),
            Value::Native(Native::Hrtime) if name == "bigint" => Ok(Value::Native(Native::HrtimeBigint)),
            Value::Func(f) if f.props.borrow().as_ref().is_some_and(|p| p.get(name).is_some()) => {
                Ok(f.props.borrow().as_ref().unwrap().get(name).unwrap())
            }
            v => Ok(method_by_name(v, name).map_or(Value::Undef, Value::Native)),
        }
    }

    /// The getter or setter function for an accessor property, if the class has one.
    fn accessor(o: &Object, kind: &str, name: &str) -> Option<u32> {
        let c = o.class.as_ref()?;
        let key = format!("{kind} {name}");
        c.methods.get(key.as_str()).copied()
    }

    #[inline(never)]
    fn get_prop_slow(&mut self, recv: &Value, site: &PropertySite) -> Result<Value, Value> {
        let name = &*site.name;
        match recv {
            Value::Obj(o) => {
                let found = o.lookup(name).map(|i| {
                    *site.cache.entry.borrow_mut() = Some((o.shape(), i));
                    o.slot(i as usize)
                });
                if let Some(x) = found {
                    return self.read_accessor(x, recv);
                }
                if let Some(x) = o.get_inherited(name) {
                    return self.read_accessor(x, recv);
                }
                if name == "constructor" {
                    return Ok(match &o.class {
                        Some(c) => Value::Class(c.clone()),
                        None => Value::Undef,
                    });
                }
                if let Some(g) = Self::accessor(o, "get", name) {
                    let f = Value::Func(Gc::new(Closure { func: g, caps: Box::new([]), props: Default::default() }));
                    return self.call(&f, recv.clone(), vec![]);
                }
                self.lookup_method(recv, name)
            }
            Value::Undef | Value::Null => Err(self.type_error(format!(
                "Cannot read properties of {} (reading '{name}')",
                recv.to_display()
            ))),
            Value::Arr(a) if name == "length" => Ok(Value::Num(a.len() as f64)),
            Value::Class(c) if name == "name" => Ok(Value::Str(JsStr::new(&*c.name))),
            Value::Func(f) if name == "prototype" => Ok(Value::Obj(crate::vm::func_prototype(f))),
            Value::Func(f) if (name == "name" || name == "length") && f.props.borrow().as_ref().is_none_or(|p| p.get(name).is_none()) => {
                let fc = &self.prog.funcs[f.func as usize];
                Ok(if name == "name" { Value::Str(JsStr::new(&*fc.name)) } else { Value::Num(fc.params.len() as f64) })
            }
            Value::Str(s) if name == "length" => Ok(Value::Num(s.len16() as f64)),
            Value::Native(Native::Hrtime) if name == "bigint" => Ok(Value::Native(Native::HrtimeBigint)),
            v => {
                let r = get_dyn(v, &site.key);
                if matches!(r, Value::Undef) {
                    return Ok(method_by_name(v, name).map_or(Value::Undef, Value::Native));
                }
                Ok(r)
            }
        }
    }

    #[inline(never)]
    fn set_prop(&mut self, recv: &Value, site: &PropertySite, val: Value) -> Result<(), Value> {
        match recv {
            Value::Obj(o) if o.lookup(&site.name).is_none() && Self::accessor(o, "set", &site.name).is_some() => {
                let s = Self::accessor(o, "set", &site.name).unwrap();
                let f = Value::Func(Gc::new(Closure { func: s, caps: Box::new([]), props: Default::default() }));
                self.call(&f, recv.clone(), vec![val])?;
                Ok(())
            }
            Value::Obj(o) if matches!(o.get(&site.name), Some(Value::Accessor(_))) => {
                let Some(Value::Accessor(ref a)) = o.get(&site.name) else { unreachable!() };
                if matches!(a.set, Value::Undef) {
                    return Err(self.type_error(format!("Cannot set property {} of {} which has only a getter", site.name, self.inspect(recv))));
                }
                self.call(&a.set, recv.clone(), vec![val])?;
                Ok(())
            }
            Value::Obj(o) => {
                if val.is_container() {
                    o.header().clear(crate::runtime::heap::flag::ACYCLIC);
                }
                let hit = matches!(&*site.cache.entry.borrow(), Some((sh, _)) if o.has_shape(sh));
                if hit {
                    let slot = site.cache.entry.borrow().as_ref().unwrap().1;
                    o.set_slot(slot as usize, val);
                    return Ok(());
                }
                if let Some(i) = o.lookup(&site.name) {
                    *site.cache.entry.borrow_mut() = Some((o.shape(), i));
                    o.set_slot(i as usize, val);
                } else {
                    o.push(&site.name, val);
                }
                Ok(())
            }
            Value::Undef | Value::Null => Err(self.type_error(format!(
                "Cannot set properties of {} (setting '{}')",
                recv.to_display(),
                site.name
            ))),
            v => {
                if !set_dyn(v, &site.key, val) {
                    return Err(self.type_error(format!("Cannot set property '{}' on {}", site.name, v.type_of())));
                }
                Ok(())
            }
        }
    }

    /// ToString, calling a user-defined `toString()` on objects.
    #[inline(never)]
    pub fn to_string_value(&mut self, v: &Value) -> Result<Gc<JsStr>, Value> {
        if let Value::Obj(o) = v {
            let user = match o.get_chain("toString") {
                Some(f) => Some(f),
                None => match &o.class {
                    Some(_) => self.lookup_method(v, "toString").ok().filter(|f| matches!(f, Value::Func(_))),
                    None => None,
                },
            };
            if let Some(f) = user.filter(task::is_callable) {
                return Ok(self.call(&f, v.clone(), vec![])?.to_js_string());
            }
        }
        Ok(v.to_js_string())
    }

    /// A property slot's value as read: an accessor calls its getter.
    #[inline(never)]
    fn read_accessor(&mut self, x: Value, recv: &Value) -> Result<Value, Value> {
        match x {
            Value::Accessor(ref a) => match &a.get {
                Value::Undef => Ok(Value::Undef),
                g => self.call(&g.clone(), recv.clone(), vec![]),
            },
            x => Ok(x),
        }
    }

    /// Pop the current frame and deliver its return value. Returns true if the
    /// run loop with base `stop` should exit.
    fn do_return(&mut self, val: Value, stop: usize) -> Result<bool, Value> {
        let fr = self.frames.pop().unwrap();
        // Release the frame's references now: deterministic destruction.
        self.release_frame(&fr);
        self.pop_regions(self.frames.len());
        let val = match self.pop_co(self.frames.len()) {
            Some(co) => self.complete_coroutine(&co, Ok(val)),
            None => val,
        };
        self.deliver(fr.ret, val)?;
        Ok(self.frames.len() <= stop)
    }

    /// Like do_return for scalar returns without boxing in the common case.
    #[inline]
    fn do_return_s(&mut self, k: Kind, bits: u64, stop: usize) -> Result<bool, Value> {
        if let Some(fr) = self.frames.last()
            && let Ret::ToSlot(rk, d) = fr.ret
            && rk == k
            && !self.is_co(self.frames.len() - 1)
        {
            let fr = self.frames.pop().unwrap();
            self.release_frame(&fr);
            self.pop_regions(self.frames.len());
            let top = self.frames.last().unwrap();
            self.scalars[top.scalar_base + d as usize] = bits;
            return Ok(self.frames.len() <= stop);
        }
        let v = match k {
            Kind::Num => Value::Num(from_bits(bits)),
            _ => Value::Bool(bits != 0),
        };
        self.do_return(v, stop)
    }

    /// Like do_return_s for a BigInt.
    #[inline]
    fn do_return_big(&mut self, n: i128, stop: usize) -> Result<bool, Value> {
        if let Some(fr) = self.frames.last()
            && let Ret::ToSlot(Kind::Big, d) = fr.ret
            && !self.is_co(self.frames.len() - 1)
        {
            let fr = self.frames.pop().unwrap();
            self.release_frame(&fr);
            self.pop_regions(self.frames.len());
            let top = self.frames.last().unwrap();
            write_big(&mut self.scalars, top.scalar_base + d as usize, n);
            return Ok(self.frames.len() <= stop);
        }
        self.do_return(Value::bigint(n), stop)
    }

    /// Whether frame `fi` runs a coroutine.
    #[inline(always)]
    fn is_co(&self, fi: usize) -> bool {
        self.co_frames.last().is_some_and(|(i, _)| *i == fi)
    }

    /// The coroutine of frame `fi` (just popped), if it had one.
    #[inline(always)]
    fn pop_co(&mut self, fi: usize) -> Option<Gc<Coroutine>> {
        if self.is_co(fi) { self.co_frames.pop().map(|(_, c)| c) } else { None }
    }

    /// The coroutine running in frame `fi`.
    fn frame_co(&self, fi: usize) -> Option<Gc<Coroutine>> {
        self.co_frames.last().filter(|(i, _)| *i == fi).map(|(_, c)| c.clone())
    }

    /// Find the handler for `err`. Ok(false): continue in the top frame;
    /// Ok(true): the run loop with base `stop` is finished (an async function
    /// turned the exception into a rejected promise).
    fn unwind(&mut self, err: Value, stop: usize) -> Result<bool, Value> {
        loop {
            let top = self.frames.len() - 1;
            if let Some(&(fidx, catch, exc)) = self.handlers.last()
                && fidx == top
            {
                self.handlers.pop();
                let fr = &mut self.frames[top];
                fr.pc = catch as usize;
                let vb = fr.value_base;
                self.values[vb + exc.index()] = err;
                return Ok(false);
            }
            let fr = self.frames.pop().unwrap();
            self.release_frame(&fr);
            self.pop_regions(self.frames.len());
            let co = self.pop_co(self.frames.len());
            if let Some(co) = &co
                && co.kind != CoKind::Generator
            {
                let out = self.complete_coroutine(co, Err(err));
                self.deliver(fr.ret, out)?;
                return Ok(self.frames.len() <= stop);
            }
            if self.frames.len() <= stop {
                return Err(err);
            }
        }
    }

    // ───────────── the interpreter loop ─────────────

    fn run(&mut self, stop: usize) -> Result<(), Value> {
        'outer: loop {
            let fi = self.frames.len() - 1;
            let func: &FunctionCode = self.frames[fi].func.get();
            let code = &func.code[..];
            let mut pc = self.frames[fi].pc;
            let sb = self.frames[fi].scalar_base;
            let vb = self.frames[fi].value_base;

            // Slot access is unchecked: codegen guarantees every slot
            // index is below the function's ns/nv, and `ensure` checked the
            // frame fits before it was pushed. The stacks never move.
            let sp: *mut u64 = unsafe { self.scalars.as_mut_ptr().add(sb) };
            let vp: *mut Value = unsafe { self.values.as_mut_ptr().add(vb) };
            // `scalar!(slot)`, `value!(slot)`: this frame's slot, read or written.
            // `number!(slot)`: a number slot's f64.
            macro_rules! scalar {
                ($r:expr) => {
                    *unsafe { &mut *sp.add(SlotIndex::index($r)) }
                };
            }
            macro_rules! value {
                ($r:expr) => {
                    *unsafe { &mut *vp.add(SlotIndex::index($r)) }
                };
            }
            macro_rules! number {
                ($r:expr) => {
                    from_bits(unsafe { *sp.add(SlotIndex::index($r)) })
                };
            }
            // `big!(slot)`: a bigint slot's i128; `set_big!(slot, n)` writes one.
            macro_rules! big {
                ($r:expr) => {{
                    let i = SlotIndex::index($r);
                    unsafe { ((*sp.add(i) as u128) | ((*sp.add(i + 1) as u128) << 64)) as i128 }
                }};
            }
            macro_rules! set_big {
                ($r:expr, $n:expr) => {{
                    let i = SlotIndex::index($r);
                    let n: i128 = $n;
                    unsafe {
                        *sp.add(i) = n as u64;
                        *sp.add(i + 1) = ((n as u128) >> 64) as u64;
                    }
                }};
            }
            debug_assert!(sb + func.scalar_slots <= self.scalars.len() && vb + func.value_slots <= self.values.len());
            macro_rules! tri {
                ($e:expr) => {
                    match $e {
                        Ok(x) => x,
                        Err(e) => break e,
                    }
                };
            }

            let err: Value = loop {
                debug_assert!(pc < code.len());
                // Every function ends in a return, so pc never runs off the end.
                let ins = unsafe { *code.get_unchecked(pc) };
                pc += 1;
                #[cfg(feature = "profile")]
                if let Some(p) = &mut self.profile {
                    p.tick(ins.kind());
                }
                match ins {
                    Instruction::LoadNumber { output, value } => scalar!(output) = bits(value),
                    Instruction::LoadBool { output, value } => scalar!(output) = value as u64,
                    Instruction::CopyNumber { to, from } => scalar!(to) = scalar!(from),
                    Instruction::CopyBool { to, from } => scalar!(to) = scalar!(from),
                    Instruction::Arithmetic { op: NumOp::Add, output, left, right } => scalar!(output) = bits(number!(left) + number!(right)),
                    Instruction::Arithmetic { op: NumOp::Sub, output, left, right } => scalar!(output) = bits(number!(left) - number!(right)),
                    Instruction::Arithmetic { op: NumOp::Mul, output, left, right } => scalar!(output) = bits(number!(left) * number!(right)),
                    Instruction::Arithmetic { op: NumOp::Div, output, left, right } => scalar!(output) = bits(number!(left) / number!(right)),
                    Instruction::Arithmetic { op: NumOp::Mod, output, left, right } => scalar!(output) = bits(js_mod(number!(left), number!(right))),
                    Instruction::AddConstant { output, left, constant } => scalar!(output) = bits(number!(left) + constant),
                    Instruction::Arithmetic { op, output, left, right } => scalar!(output) = bits(num_op(op, number!(left), number!(right))),
                    Instruction::Compare { op: CmpOp::Lt, output, left, right } => scalar!(output) = (number!(left) < number!(right)) as u64,
                    Instruction::Compare { op: CmpOp::Le, output, left, right } => scalar!(output) = (number!(left) <= number!(right)) as u64,
                    Instruction::Compare { op: CmpOp::Gt, output, left, right } => scalar!(output) = (number!(left) > number!(right)) as u64,
                    Instruction::Compare { op: CmpOp::Ge, output, left, right } => scalar!(output) = (number!(left) >= number!(right)) as u64,
                    Instruction::Compare { op: CmpOp::Eq, output, left, right } => scalar!(output) = (number!(left) == number!(right)) as u64,
                    Instruction::Compare { op: CmpOp::Ne, output, left, right } => scalar!(output) = (number!(left) != number!(right)) as u64,
                    Instruction::CompareBools { equal: true, output, left, right } => scalar!(output) = (scalar!(left) == scalar!(right)) as u64,
                    Instruction::CompareBools { equal: false, output, left, right } => scalar!(output) = (scalar!(left) != scalar!(right)) as u64,
                    Instruction::Negate { output, value } => scalar!(output) = bits(-number!(value)),
                    Instruction::Not { output, value } => scalar!(output) = (scalar!(value) == 0) as u64,
                    Instruction::BitNot { output, value } => scalar!(output) = bits(!to_int32(number!(value)) as f64),
                    Instruction::NumberIsTruthy { output, value } => {
                        let x = number!(value);
                        scalar!(output) = (x != 0.0 && !x.is_nan()) as u64
                    }
                    Instruction::MathUnary { function, output, value } => scalar!(output) = bits(natives::math1(function, number!(value))),
                    Instruction::MathBinary { function, output, left, right } => scalar!(output) = bits(natives::math2(function, number!(left), number!(right))),

                    Instruction::LoadBigInt { constant, output } => {
                        let Value::BigInt(n) = &func.constants[constant as usize] else { unreachable!() };
                        set_big!(output, **n)
                    }
                    Instruction::CopyBigInt { from, to } => set_big!(to, big!(from)),
                    Instruction::BigIntArithmetic { op, left, right, output } => match bigint::arithmetic(op, big!(left), big!(right)) {
                        Ok(n) => set_big!(output, n),
                        Err(e) => break self.bigint_error(e),
                    },
                    Instruction::CompareBigInts { op, left, right, output } => scalar!(output) = bigint::compare(op, big!(left), big!(right)) as u64,
                    Instruction::NegateBigInt { value, output } => match bigint::negate(big!(value)) {
                        Ok(n) => set_big!(output, n),
                        Err(e) => break self.bigint_error(e),
                    },
                    Instruction::BitNotBigInt { value, output } => set_big!(output, !big!(value)),
                    Instruction::BigIntIsTruthy { value, output } => scalar!(output) = (big!(value) != 0) as u64,
                    Instruction::BoxBigInt { value, output } => value!(output) = Value::bigint(big!(value)),
                    Instruction::UnboxBigInt { value, output } => {
                        let n = match &value!(value) {
                            Value::BigInt(n) => **n,
                            other => {
                                let m = format!("expected a bigint, got {}", describe(other));
                                break self.type_error(m);
                            }
                        };
                        set_big!(output, n)
                    }
                    Instruction::ReadGlobalBigInt { global, output } => set_big!(output, read_big(&self.global_scalars, global as usize)),
                    Instruction::WriteGlobalBigInt { global, value } => write_big(&mut self.global_scalars, global as usize, big!(value)),
                    Instruction::ReturnBigInt { value } => {
                        if tri!(self.do_return_big(big!(value), stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }

                    Instruction::Jump { to } => pc = to as usize,
                    Instruction::JumpIf { when: true, condition, to } => {
                        if scalar!(condition) != 0 {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIf { when: false, condition, to } => {
                        if scalar!(condition) == 0 {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIfCompare { op, when, left, right, to } => {
                        if cmp(op, number!(left), number!(right)) == when {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIfTruthy { when: true, value, to } => {
                        if value!(value).truthy() {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIfTruthy { when: false, value, to } => {
                        if !value!(value).truthy() {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIfNullish { when: true, value, to } => {
                        if value!(value).is_nullish() {
                            pc = to as usize
                        }
                    }
                    Instruction::JumpIfNullish { when: false, value, to } => {
                        if !value!(value).is_nullish() {
                            pc = to as usize
                        }
                    }

                    Instruction::LoadConstant { output, constant } => value!(output) = func.constants[constant as usize].clone(),
                    Instruction::LoadUndefined { output } => value!(output) = Value::Undef,
                    Instruction::LoadNull { output } => value!(output) = Value::Null,
                    Instruction::CopyValue { to, from } => value!(to) = value!(from).clone(),
                    Instruction::Release { first, count } => {
                        for x in &mut self.values[vb + first.index()..vb + first.index() + count as usize] {
                            x.release();
                        }
                    }
                    Instruction::BoxNumber { output, value } => value!(output) = Value::Num(number!(value)),
                    Instruction::BoxBool { output, value } => value!(output) = Value::Bool(scalar!(value) != 0),
                    Instruction::UnboxNumber { output, value } => {
                        let x = match &value!(value) {
                            Value::Num(x) => *x,
                            other => {
                                let m = format!("expected a number, got {}", describe(other));
                                break self.type_error(m);
                            }
                        };
                        scalar!(output) = bits(x)
                    }
                    Instruction::UnboxBool { output, value } => {
                        let x = match &value!(value) {
                            Value::Bool(x) => *x,
                            other => {
                                let m = format!("expected a boolean, got {}", describe(other));
                                break self.type_error(m);
                            }
                        };
                        scalar!(output) = x as u64
                    }
                    Instruction::IsTruthy { output, value } => scalar!(output) = value!(value).truthy() as u64,
                    Instruction::NumberToString { output, value } => value!(output) = Value::str(num_to_string(number!(value))),
                    Instruction::ToString { output, value } => {
                        let x = match &value!(value) {
                            v @ Value::Obj(_) => {
                                let v = v.clone();
                                self.frames[fi].pc = pc;
                                let r = self.to_string_value(&v);
                                                                tri!(r)
                            }
                            v => v.to_js_string(),
                        };
                        value!(output) = Value::Str(x)
                    }
                    Instruction::ToNumber { output, value } => {
                        if let Value::BigInt(_) = value!(value) {
                            break self.bigint_error(bigint::TO_NUMBER);
                        }
                        scalar!(output) = bits(value!(value).to_number())
                    }
                    Instruction::Concat { output, left, right } => {
                        let r = match (&value!(left), &value!(right)) {
                            (Value::Str(x), Value::Str(y)) => {
                                if y.s.is_empty() {
                                    Value::Str(x.clone())
                                } else if x.s.is_empty() {
                                    Value::Str(y.clone())
                                } else {
                                    let mut st = String::with_capacity(x.s.len() + y.s.len());
                                    st.push_str(&x.s);
                                    st.push_str(&y.s);
                                    Value::str(st)
                                }
                            }
                            (x, y) => Value::str(format!("{}{}", x.to_js_string().s, y.to_js_string().s)),
                        };
                        value!(output) = r
                    }
                    Instruction::CompareStrings { op, output, left, right } => {
                        let r = match (&value!(left), &value!(right)) {
                            (Value::Str(x), Value::Str(y)) => str_cmp(op, &x.s, &y.s),
                            (x, y) => str_cmp(op, &x.to_js_string().s, &y.to_js_string().s),
                        };
                        scalar!(output) = r as u64
                    }
                    Instruction::StrictEquals { output, left, right, equal } => scalar!(output) = (value!(left).strict_eq(&value!(right)) == equal) as u64,
                    Instruction::LooseEquals { output, left, right, equal } => scalar!(output) = (value!(left).loose_eq(&value!(right)) == equal) as u64,
                    Instruction::DynamicAdd { output, left, right } => {
                        let r = match (&value!(left), &value!(right)) {
                            (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                            (Value::BigInt(x), Value::BigInt(y)) => match bigint::arithmetic(NumOp::Add, **x, **y) {
                                Ok(n) => Value::bigint(n),
                                Err(e) => break self.bigint_error(e),
                            },
                            (x @ Value::Str(_), y) | (x, y @ Value::Str(_)) => {
                                Value::str(format!("{}{}", x.to_js_string().s, y.to_js_string().s))
                            }
                            (x @ (Value::Arr(_) | Value::Obj(_)), y) | (x, y @ (Value::Arr(_) | Value::Obj(_))) => {
                                let (x, y) = (x.clone(), y.clone());
                                self.frames[fi].pc = pc;
                                let a = self.to_string_value(&x);
                                let b = a.and_then(|a| self.to_string_value(&y).map(|b| (a, b)));
                                                                let (a, b) = tri!(b);
                                Value::str(format!("{}{}", a.s, b.s))
                            }
                            (Value::BigInt(_), _) | (_, Value::BigInt(_)) => break self.bigint_error(bigint::MIX),
                            (x, y) => Value::Num(x.to_number() + y.to_number()),
                        };
                        value!(output) = r
                    }
                    Instruction::DynamicArithmetic { op, output, left, right } => {
                        if matches!(value!(left), Value::BigInt(_)) || matches!(value!(right), Value::BigInt(_)) {
                            break self.bigint_error(bigint::TO_NUMBER);
                        }
                        scalar!(output) = bits(dyn_num(op, &value!(left), &value!(right)))
                    }
                    Instruction::DynamicArithmeticToValue { op, output, left, right } => match dyn_numeric(op, &value!(left), &value!(right)) {
                        Ok(r) => value!(output) = r,
                        Err(e) => break self.bigint_error(e),
                    },
                    Instruction::DynamicAddConstant { output, value, constant } => {
                        let r = match &value!(value) {
                            Value::Num(x) => Value::Num(x + constant),
                            Value::BigInt(x) => match bigint::arithmetic(NumOp::Add, **x, constant as i128) {
                                Ok(n) => Value::bigint(n),
                                Err(e) => break self.bigint_error(e),
                            },
                            x => Value::Num(x.to_number() + constant),
                        };
                        value!(output) = r
                    }
                    Instruction::DynamicCompare { op, output, left, right } => scalar!(output) = dyn_compare(op, &value!(left), &value!(right)) as u64,
                    Instruction::DynamicNegate { output, value } => {
                        let r = match &value!(value) {
                            Value::BigInt(n) => match bigint::negate(**n) {
                                Ok(n) => Value::bigint(n),
                                Err(e) => break self.bigint_error(e),
                            },
                            v => Value::Num(-v.to_number()),
                        };
                        value!(output) = r
                    }
                    Instruction::DynamicBitNot { output, value } => {
                        let r = match &value!(value) {
                            Value::BigInt(n) => Value::bigint(!**n),
                            v => Value::Num(!to_int32(v.to_number()) as f64),
                        };
                        value!(output) = r
                    }
                    Instruction::TypeOf { output, value } => value!(output) = Value::str(value!(value).type_of()),
                    Instruction::In { output: d, key: k, object: o } => {
                        let r = match &value!(o) {
                            Value::Obj(ob) => {
                                let key = value!(k).to_js_string();
                                ob.get_chain(&key.s).is_some()
                                    || ob.class.as_ref().is_some_and(|c| {
                                        let mut c = Some(c.clone());
                                        while let Some(k) = c {
                                            if k.methods.contains_key(&*key.s) {
                                                return true;
                                            }
                                            c = k.parent.clone();
                                        }
                                        false
                                    })
                            }
                            Value::Arr(a) => as_index(&value!(k)).is_some_and(|i| i < a.len()) || value!(k).to_display() == "length",
                            other => {
                                let m = format!("Cannot use 'in' operator to search for a key in {}", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        scalar!(d) = r as u64
                    }
                    Instruction::InstanceOf { output, value, class } => {
                        scalar!(output) = matches!(&value!(value), Value::Obj(ob) if ob.class.as_ref().is_some_and(|c| c.is_subclass_of(class)))
                            as u64
                    }

                    Instruction::NewCell { output } => value!(output) = Value::Cell(Gc::new(std::cell::RefCell::new(Value::Undef))),
                    Instruction::ReadCell { output: d, cell: c } => {
                        let x = match &value!(c) {
                            Value::Cell(cell) => cell.borrow().clone(),
                            _ => unreachable!("not a cell"),
                        };
                        value!(d) = x
                    }
                    Instruction::WriteCell { cell: c, value: s } => {
                        let x = value!(s).clone();
                        match &value!(c) {
                            Value::Cell(cell) => *cell.borrow_mut() = x,
                            _ => unreachable!("not a cell"),
                        }
                    }
                    Instruction::ReadCapture { output, capture } => {
                        let x = self.frames[fi].closure.as_ref().unwrap().caps[capture as usize].borrow().clone();
                        value!(output) = x
                    }
                    Instruction::WriteCapture { capture, value } => {
                        let x = value!(value).clone();
                        *self.frames[fi].closure.as_ref().unwrap().caps[capture as usize].borrow_mut() = x;
                    }
                    Instruction::ReadGlobalNumber { output, global } => scalar!(output) = self.global_scalars[global as usize],
                    Instruction::ReadGlobalBool { output, global } => scalar!(output) = self.global_scalars[global as usize],
                    Instruction::WriteGlobalNumber { global, value } => self.global_scalars[global as usize] = scalar!(value),
                    Instruction::WriteGlobalBool { global, value } => self.global_scalars[global as usize] = scalar!(value),
                    Instruction::ReadGlobal { output, global } => value!(output) = self.global_values[global as usize].clone(),
                    Instruction::WriteGlobal { global, value } => self.global_values[global as usize] = value!(value).clone(),

                    Instruction::MakeClosure { output, function, captures } => {
                        let list = &func.capture_lists[captures as usize];
                        let mut cells = Vec::with_capacity(list.len());
                        for c in list {
                            cells.push(match c {
                                CaptureFrom::Slot(r) => match &value!(*r) {
                                    Value::Cell(cell) => cell.clone(),
                                    _ => unreachable!("capture source is not a cell"),
                                },
                                CaptureFrom::Capture(i) => self.frames[fi].closure.as_ref().unwrap().caps[*i as usize].clone(),
                            });
                        }
                        value!(output) = Value::Func(Gc::new(Closure {
                            func: function,
                            caps: cells.into_boxed_slice(),
                            props: Default::default(),
                        }))
                    }
                    Instruction::Call { site } => {
                        self.frames[fi].pc = pc;
                        let r = self.do_call(func, &func.call_sites[site as usize], sb, vb);
                                                match r {
                            Ok(true) => continue 'outer,
                            Ok(false) => {}
                            Err(e) => break e,
                        }
                    }
                    Instruction::ReturnNumber { value } => {
                        if tri!(self.do_return_s(Kind::Num, scalar!(value), stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    Instruction::ReturnBool { value } => {
                        if tri!(self.do_return_s(Kind::Bool, scalar!(value), stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    Instruction::ReturnValue { value } => {
                        // A borrowed slot's value is not ours to move: count a copy.
                        let x = if value.index() < 64 && func.borrowed & (1 << value.index()) != 0 {
                            value!(value).clone()
                        } else {
                            std::mem::take(&mut value!(value))
                        };
                        if tri!(self.do_return(x, stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    Instruction::ReturnUndefined => {
                        if tri!(self.do_return(Value::Undef, stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }

                    Instruction::NewArray { output, numbers } => {
                        value!(output) = Value::Arr(if numbers { Array::new_num(Vec::new()) } else { Array::new_val(Vec::new()) })
                    }
                    Instruction::PushNumber { array, value } => {
                        let x = number!(value);
                        match &value!(array) {
                            Value::Arr(arr) => {
                                let mut st = arr.store.borrow_mut();
                                match &mut *st {
                                    ArrStore::Num(vec) => vec.push(x),
                                    ArrStore::Val(vec) => vec.push(Value::Num(x)),
                                }
                            }
                            other => {
                                let m = format!("cannot push to {}", describe(other));
                                break self.type_error(m);
                            }
                        }
                    }
                    Instruction::PushValue { array, value } => {
                        let x = value!(value).clone();
                        match &value!(array) {
                            Value::Arr(arr) => arr.push(x),
                            other => {
                                let m = format!("cannot push to {}", describe(other));
                                break self.type_error(m);
                            }
                        }
                    }
                    Instruction::PushSpread { array, source } => {
                        let src = value!(source).clone();
                        let items = tri!(natives::iterate(self, &src));
                        if let Value::Arr(arr) = &value!(array) {
                            for x in items {
                                arr.push(x);
                            }
                        }
                    }
                    Instruction::ReadElementNumber { output: d, array: a, index: i } => {
                        let idx = number!(i);
                        let x = match &value!(a) {
                            Value::Arr(arr) => {
                                let st = arr.store.borrow();
                                match &*st {
                                    ArrStore::Num(vec) if idx >= 0.0 && (idx as usize) < vec.len() && idx.fract() == 0.0 => {
                                        Ok(vec[idx as usize])
                                    }
                                    ArrStore::Num(_) => Ok(f64::NAN),
                                    ArrStore::Val(vec) => {
                                        match if idx >= 0.0 && idx.fract() == 0.0 { vec.get(idx as usize) } else { None } {
                                            Some(Value::Num(x)) => Ok(*x),
                                            None | Some(Value::Undef) => Ok(f64::NAN),
                                            Some(other) => Err(format!("expected a number element, got {}", describe(other))),
                                        }
                                    }
                                }
                            }
                            other => Err(format!("cannot index {}", describe(other))),
                        };
                        match x {
                            Ok(x) => scalar!(d) = bits(x),
                            Err(m) => break self.type_error(m),
                        }
                    }
                    Instruction::WriteElementNumber { array: a, index: i, value: s } => {
                        let idx = number!(i);
                        let x = number!(s);
                        match &value!(a) {
                            Value::Arr(arr) => {
                                let mut st = arr.store.borrow_mut();
                                match &mut *st {
                                    ArrStore::Num(vec) if idx >= 0.0 && (idx as usize) < vec.len() && idx.fract() == 0.0 => {
                                        vec[idx as usize] = x
                                    }
                                    _ => {
                                        drop(st);
                                        if idx >= 0.0 && idx.fract() == 0.0 {
                                            arr.set(idx as usize, Value::Num(x));
                                        }
                                    }
                                }
                            }
                            other => {
                                let m = format!("cannot index {}", describe(other));
                                break self.type_error(m);
                            }
                        }
                    }
                    Instruction::ReadElement { output, array, index } => {
                        let idx = number!(index);
                        let x = match &value!(array) {
                            Value::Arr(arr) => {
                                if idx >= 0.0 && idx.fract() == 0.0 {
                                    arr.get(idx as usize)
                                } else {
                                    Value::Undef
                                }
                            }
                            Value::Str(st) => {
                                if idx >= 0.0 && idx.fract() == 0.0 && (idx as usize) < st.len16() {
                                    Value::str(st.slice16(idx as usize, idx as usize + 1))
                                } else {
                                    Value::Undef
                                }
                            }
                            other @ (Value::Undef | Value::Null) => {
                                let m = format!("Cannot read properties of {} (reading '{}')", other.to_display(), num_to_string(idx));
                                break self.type_error(m);
                            }
                            other => get_dyn(other, &Value::Num(idx)),
                        };
                        value!(output) = x
                    }
                    Instruction::WriteElement { array, index, value } => {
                        let idx = number!(index);
                        let x = value!(value).clone();
                        match &value!(array) {
                            Value::Arr(arr) => {
                                if idx >= 0.0 && idx.fract() == 0.0 {
                                    arr.set(idx as usize, x)
                                }
                            }
                            other => {
                                if !set_dyn(other, &Value::Num(idx), x) {
                                    let m = format!("Cannot set properties of {}", describe(other));
                                    break self.type_error(m);
                                }
                            }
                        }
                    }
                    Instruction::ReadDynamic { output: d, object: o, key: k } => {
                        // Array element by integer index.
                        if let (Value::Arr(a), Value::Num(i)) = (&value!(o), &value!(k))
                            && *i >= 0.0
                            && (*i as usize) as f64 == *i
                        {
                            let x = a.get(*i as usize);
                            value!(d) = x;
                            continue;
                        }
                        let x = match &value!(o) {
                            other @ (Value::Undef | Value::Null) => {
                                let m = format!(
                                    "Cannot read properties of {} (reading '{}')",
                                    other.to_display(),
                                    value!(k).to_display()
                                );
                                break self.type_error(m);
                            }
                            Value::Obj(ob) => {
                                let key = value!(k).to_js_string();
                                match ob.get(&key.s) {
                                    Some(Value::Accessor(ref a)) => {
                                        let recv = value!(o).clone();
                                        self.frames[fi].pc = pc;
                                        let r = self.read_accessor(Value::Accessor(a.clone()), &recv);
                                                                                tri!(r)
                                    }
                                    Some(x) => x,
                                    None => tri!(self.lookup_method(&value!(o).clone(), &key.s.clone())),
                                }
                            }
                            other => {
                                let r = get_dyn(other, &value!(k));
                                if matches!(r, Value::Undef)
                                    && let Value::Str(name) = &value!(k)
                                {
                                    method_by_name(other, &name.s).map_or(Value::Undef, Value::Native)
                                } else {
                                    r
                                }
                            }
                        };
                        value!(d) = x
                    }
                    Instruction::WriteDynamic { object, key, value } => {
                        let x = value!(value).clone();
                        if !set_dyn(&value!(object), &value!(key), x) {
                            let m = format!(
                                "Cannot set properties of {} (setting '{}')",
                                value!(object).to_display(),
                                value!(key).to_display()
                            );
                            break self.type_error(m);
                        }
                    }
                    Instruction::DeleteDynamic { object: o, key: k } => {
                        if let Value::Obj(ob) = &value!(o) {
                            let key = value!(k).to_js_string();
                            ob.delete(&key.s);
                        }
                    }
                    Instruction::ArrayLength { output, array } => {
                        let x = match &value!(array) {
                            Value::Arr(arr) => arr.len(),
                            Value::Str(st) => st.len16(),
                            other => {
                                let m = format!("Cannot read properties of {} (reading 'length')", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        scalar!(output) = bits(x as f64)
                    }
                    Instruction::SetArrayLength { array: a, length: s } => {
                        let x = number!(s);
                        if let Value::Arr(arr) = &value!(a) {
                            if x < 0.0 || x.fract() != 0.0 {
                                break self.range_error("Invalid array length".into());
                            }
                            arr.set_len(x as usize)
                        }
                    }
                    Instruction::StringLength { output, string } => {
                        let x = match &value!(string) {
                            Value::Str(st) => st.len16(),
                            other => {
                                let m = format!("Cannot read properties of {} (reading 'length')", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        scalar!(output) = bits(x as f64)
                    }
                    Instruction::CharCodeAt { output, string, index } => {
                        let idx = number!(index);
                        let x = match &value!(string) {
                            Value::Str(st) => {
                                if idx >= 0.0 && idx.fract() == 0.0 {
                                    st.code_unit(idx as usize).map_or(f64::NAN, |c| c as f64)
                                } else if idx.is_nan() {
                                    st.code_unit(0).map_or(f64::NAN, |c| c as f64)
                                } else {
                                    f64::NAN
                                }
                            }
                            other => {
                                let m = format!("Cannot read properties of {} (reading 'charCodeAt')", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        scalar!(output) = bits(x)
                    }
                    Instruction::ToIterationArray { output, value } => {
                        let x = match &value!(value) {
                            Value::Arr(_) => value!(value).clone(),
                            other => {
                                let other = other.clone();
                                Value::Arr(Array::new_val(tri!(natives::iterate(self, &other))))
                            }
                        };
                        value!(output) = x
                    }
                    Instruction::KeysArray { output, object } => {
                        let x = Value::Arr(Array::new_val(keys_of(&value!(object))));
                        value!(output) = x
                    }

                    Instruction::NewObject { output, shape } => {
                        let sh = self.prog.shapes[shape as usize].clone();
                        let n = sh.keys.len();
                        let _ = n;
                        value!(output) = Value::Obj(Object::blank(sh, None))
                    }
                    Instruction::SpreadIntoObject { object, source } => {
                        let src = value!(source).clone();
                        let dst = value!(object).clone();
                        for k in keys_of(&src) {
                            let x = get_dyn(&src, &k);
                            set_dyn(&dst, &k, x);
                        }
                    }
                    Instruction::ReadField { output, object, field } => {
                        let x = match &value!(object) {
                            Value::Obj(ob) => ob.slot(field as usize),
                            other => {
                                let m = format!("Cannot read properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        value!(output) = x
                    }
                    Instruction::WriteField { object, field, value } => {
                        let x = value!(value).clone();
                        match &value!(object) {
                            Value::Obj(ob) => ob.set_slot(field as usize, x),
                            other => {
                                let m = format!("Cannot set properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        }
                    }
                    Instruction::ReadFieldNumber { output: d, object: o, field: i } => {
                        let x = match &value!(o) {
                            Value::Obj(ob) => ob.slot_num(i as usize),
                            other => Err(format!("Cannot read properties of {}", other.to_display())),
                        };
                        match x {
                            Ok(x) => scalar!(d) = bits(x),
                            Err(m) => break self.type_error(m),
                        }
                    }
                    Instruction::WriteFieldNumber { object, field, value } => {
                        let x = number!(value);
                        match &value!(object) {
                            Value::Obj(ob) => ob.set_slot(field as usize, Value::Num(x)),
                            other => {
                                let m = format!("Cannot set properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        }
                    }
                    Instruction::ReadProperty { output, object, property } => {
                        let site = &func.property_sites[property as usize];
                        let fast = match &value!(object) {
                            Value::Obj(ob) => {
                                match &*site.cache.entry.borrow() {
                                    Some((sh, slot)) if ob.has_shape(sh) => Some(ob.slot(*slot as usize)),
                                    _ => None,
                                }
                            }
                            Value::Arr(a) if site.is_length => Some(Value::Num(a.len() as f64)),
                            _ => None,
                        };
                        let x = match fast {
                            Some(Value::Accessor(ref a)) => {
                                let recv = value!(object).clone();
                                self.frames[fi].pc = pc;
                                let r = self.read_accessor(Value::Accessor(a.clone()), &recv);
                                                                tri!(r)
                            }
                            Some(x) => x,
                            None => {
                                let recv = value!(object).clone();
                                self.frames[fi].pc = pc;
                                let r = self.get_prop_slow(&recv, site);
                                                                tri!(r)
                            }
                        };
                        value!(output) = x
                    }
                    Instruction::WriteProperty { object, property, value } => {
                        let site = &func.property_sites[property as usize];
                        let x = value!(value).clone();
                        // Inline cache hit: the shape already has the slot.
                        let x = if let Value::Obj(ob) = &value!(object) {
                            let hit = match &*site.cache.entry.borrow() {
                                Some((sh, slot)) if ob.has_shape(sh) && !ob.slot_is_accessor(*slot as usize) => Some(*slot),
                                _ => None,
                            };
                            match hit {
                                Some(slot) => {
                                    ob.set_slot(slot as usize, x);
                                    None
                                }
                                _ => Some(x),
                            }
                        } else {
                            Some(x)
                        };
                        if let Some(x) = x {
                            let recv = value!(object).clone();
                            self.frames[fi].pc = pc;
                            let r = self.set_prop(&recv, site, x);
                                                        tri!(r);
                        }
                    }
                    Instruction::NewInstance { output, class } => {
                        let c = self.prog.classes[class as usize].clone();
                        let n = c.shape.keys.len();
                        let _ = n;
                        value!(output) = Value::Obj(Object::blank(c.shape.clone(), Some(c)))
                    }
                    Instruction::LoadClass { output, class } => value!(output) = Value::Class(self.prog.classes[class as usize].clone()),

                    Instruction::Throw { value } => break value!(value).clone(),
                    Instruction::TryBegin { catch, exception } => self.handlers.push((fi, catch, exception)),
                    Instruction::TryEnd => {
                        self.handlers.pop();
                    }

                    Instruction::DefineAccessor { .. }
                    | Instruction::GetIterator { .. }
                    | Instruction::GetAsyncIterator { .. }
                    | Instruction::IteratorStep { .. }
                    | Instruction::IteratorNextResult { .. }
                    | Instruction::UnpackIteratorResult { .. }
                    | Instruction::IteratorClose { .. }
                    | Instruction::IteratorResume { .. }
                    | Instruction::CreateGenerator
                    | Instruction::StartAsync
                    | Instruction::Yield { .. }
                    | Instruction::Await { .. }
                    | Instruction::EnterRegion { .. }
                    | Instruction::ExitRegion
                    | Instruction::ExitRegionsTo { .. }
                    | Instruction::NewInstanceIn { .. }
                    | Instruction::NewObjectIn { .. }
                    | Instruction::NewArrayIn { .. } => {
                        self.frames[fi].pc = pc;
                        let r = self.exec_extended(fi, pc, sb, vb, stop);
                                                match tri!(r) {
                            Flow::Next => {}
                            Flow::Jump(to) => pc = to,
                            Flow::Switch => continue 'outer,
                            Flow::Return => return Ok(()),
                        }
                    }
                }
            };
            // An exception was raised in this frame (or, if it already returned, in its caller).
            if fi < self.frames.len() {
                self.frames[fi].pc = pc;
            }
            if self.unwind(err, stop)? {
                return Ok(());
            }
        }
    }
}
