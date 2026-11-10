//! The interpreter. Frames index into two shared register stacks (scalars and
//! values). Calls into JS functions don't recurse in Rust; only natives that
//! call back into JS (e.g. `arr.map(f)`) start a nested run loop.

use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use crate::bytecode::*;
use crate::hir::{CmpOp, NumOp};
use crate::runtime::natives::{self, as_index, call_native, get_dyn, js_mod, js_pow, keys_of, make_array, method_by_name, set_dyn, to_int32};
use crate::runtime::*;
use crate::types::Kind;

const MAX_DEPTH: usize = 10_000;

#[derive(Debug, Clone, Copy)]
enum Ret {
    Reg(Kind, R),
    Discard,
    /// Return to a native caller (`Vm::call`).
    Native,
}

/// A function of the running program. The program (held by the VM in an
/// `Rc`) outlives every frame, so frames borrow functions without refcounting.
#[derive(Debug, Clone, Copy)]
struct FnRef(*const FuncCode);

impl FnRef {
    fn new(f: &Rc<FuncCode>) -> FnRef {
        FnRef(Rc::as_ptr(f))
    }
    #[inline(always)]
    fn get<'a>(self) -> &'a FuncCode {
        unsafe { &*self.0 }
    }
}

#[derive(Debug)]
struct Frame {
    func: FnRef,
    pc: usize,
    sb: usize,
    vb: usize,
    closure: Option<Rc<Closure>>,
    ret: Ret,
}

pub struct Vm {
    pub prog: Rc<Program>,
    s: Vec<u64>,
    v: Vec<Value>,
    gs: Vec<u64>,
    gv: Vec<Value>,
    frames: Vec<Frame>,
    /// Active try handlers: (frame index, catch pc, exception register).
    handlers: Vec<(usize, u32, R)>,
    out: std::io::BufWriter<std::io::Stdout>,
    pub start: Instant,
    native_ret: Value,
}

#[inline(always)]
fn f(bits: u64) -> f64 {
    f64::from_bits(bits)
}
#[inline(always)]
fn b(x: f64) -> u64 {
    x.to_bits()
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
    if op == NumOp::Mod { fast_mod(x, y) } else { num_op(op, x, y) }
}

/// `%` with an integer fast path (fmod is slow); same results as JS, including -0.
fn fast_mod(x: f64, y: f64) -> f64 {
    let (xi, yi) = (x as i64, y as i64);
    if xi as f64 == x && yi as f64 == y && yi != 0 && xi != i64::MIN {
        let r = xi % yi;
        if r == 0 && x < 0.0 {
            return -0.0;
        }
        return r as f64;
    }
    x % y
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

fn str_cmp(op: CmpOp, a: &str, b: &str) -> bool {
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

fn describe(v: &Value) -> String {
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
            s: vec![0; 1024],
            v: vec![Value::Undef; 1024],
            gs: vec![0; prog.n_gs],
            gv: vec![Value::Undef; prog.n_gv],
            frames: Vec::new(),
            handlers: Vec::new(),
            out: std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()),
            start: Instant::now(),
            native_ret: Value::Undef,
            prog,
        }
    }

    // ───────────── services for natives ─────────────

    pub fn print(&mut self, s: &str) {
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
        let prog = self.prog.clone();
        inspect(v, &move |id| prog.funcs[id as usize].name.clone())
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
    pub fn syntax_error(&mut self, msg: String) -> Value {
        self.make_error(self.prog.syntax_error_class.or(self.prog.error_class), msg)
    }

    // ───────────── entry points ─────────────

    /// Run the program's main function. Returns the uncaught exception, if any.
    pub fn run_main(&mut self) -> Result<(), Value> {
        let main = self.prog.funcs[self.prog.main as usize].clone();
        self.ensure(0, 0, &main);
        self.frames.push(Frame {
            func: FnRef::new(&main),
            pc: 0,
            sb: 0,
            vb: 0,
            closure: None,
            ret: Ret::Discard,
        });
        let r = self.run(0);
        self.flush();
        r
    }

    /// Call any callable value from native code.
    pub fn call(&mut self, callee: &Value, this: Value, args: Vec<Value>) -> Result<Value, Value> {
        if let Value::Native(n) = callee {
            return call_native(self, *n, this, args);
        }
        self.call_slice(callee, this, &args)
    }

    /// Call from native code with arguments borrowed from the caller (no allocation
    /// for the argument list — used by callbacks such as `map`'s).
    pub fn call_slice(&mut self, callee: &Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        let (func, closure) = match callee {
            Value::Func(c) => (self.prog.funcs[c.func as usize].clone(), Some(c.clone())),
            Value::Native(n) => return call_native(self, *n, this, args.to_vec()),
            v => return Err(self.type_error(format!("{} is not a function", describe(v)))),
        };
        if self.frames.len() >= MAX_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        let (sb, vb) = match self.frames.last() {
            Some(top) => (top.sb + top.func.get().ns, top.vb + top.func.get().nv),
            None => (0, 0),
        };
        self.ensure(sb, vb, &func);
        self.v[vb] = this;
        if func.rest {
            self.bind_values(&func, sb, vb, args.to_vec())?;
        } else {
            self.bind_slice(&func, sb, vb, args)?;
        }
        let depth = self.frames.len();
        self.frames.push(Frame {
            func: FnRef::new(&func),
            pc: 0,
            sb,
            vb,
            closure,
            ret: Ret::Native,
        });
        self.run(depth)?;
        Ok(std::mem::take(&mut self.native_ret))
    }

    fn ensure(&mut self, sb: usize, vb: usize, func: &FuncCode) {
        let need_s = sb + func.ns + 1;
        if self.s.len() < need_s {
            self.s.resize(need_s.max(self.s.len() * 2), 0);
        }
        let need_v = vb + func.nv + 1;
        if self.v.len() < need_v {
            self.v.resize(need_v.max(self.v.len() * 2), Value::Undef);
        }
    }

    /// Bind boxed argument values to a callee's parameter registers.
    fn bind_values(&mut self, func: &FuncCode, sb: usize, vb: usize, mut args: Vec<Value>) -> Result<(), Value> {
        let np = func.params.len();
        if func.rest && np > 0 {
            let rest: Vec<Value> = if args.len() >= np { args.split_off(np - 1) } else { vec![] };
            args.truncate(np - 1);
            args.resize(np - 1, Value::Undef);
            args.push(Value::Arr(make_array(rest)));
        }
        self.bind_slice(func, sb, vb, &args)
    }

    /// Bind argument values (no rest packing) to a callee's parameter registers.
    fn bind_slice(&mut self, func: &FuncCode, sb: usize, vb: usize, args: &[Value]) -> Result<(), Value> {
        let (mut si, mut vi) = (0usize, 1usize);
        for (i, k) in func.params.iter().enumerate() {
            let a = args.get(i);
            match k {
                Kind::Val => {
                    self.v[vb + vi] = a.cloned().unwrap_or(Value::Undef);
                    vi += 1;
                }
                k => {
                    self.s[sb + si] = match a {
                        Some(a) => self.unbox(*k, a)?,
                        None if *k == Kind::Num => b(f64::NAN),
                        None => 0,
                    };
                    si += 1;
                }
            }
        }
        Ok(())
    }

    fn unbox(&mut self, k: Kind, v: &Value) -> Result<u64, Value> {
        match (k, v) {
            (Kind::Num, Value::Num(n)) => Ok(b(*n)),
            (Kind::Bool, Value::Bool(x)) => Ok(*x as u64),
            (Kind::Num, v) => Err(self.type_error(format!("expected a number, got {}", describe(v)))),
            (Kind::Bool, v) => Err(self.type_error(format!("expected a boolean, got {}", describe(v)))),
            (Kind::Val, _) => unreachable!(),
        }
    }

    fn boxed(k: Kind, bits: u64) -> Value {
        match k {
            Kind::Num => Value::Num(f(bits)),
            Kind::Bool => Value::Bool(bits != 0),
            Kind::Val => unreachable!(),
        }
    }

    /// Read a caller register of the given kind as a Value.
    fn read_val(&self, k: Kind, r: R, sb: usize, vb: usize) -> Value {
        match k {
            Kind::Val => self.v[vb + r as usize].clone(),
            k => Self::boxed(k, self.s[sb + r as usize]),
        }
    }

    // ───────────── calls ─────────────

    /// Perform a call. Returns Ok(true) if a JS frame was pushed.
    fn do_call(&mut self, caller: &FuncCode, ci: &CallInfo, sb: usize, vb: usize) -> Result<bool, Value> {
        let this = ci.this.map(|r| self.v[vb + r as usize].clone());
        let (func, closure) = match &ci.target {
            CallTarget::Direct(fi) => (self.prog.funcs[*fi as usize].clone(), None),
            CallTarget::Value(r) => match &self.v[vb + *r as usize] {
                Value::Func(c) => (self.prog.funcs[c.func as usize].clone(), Some(c.clone())),
                Value::Native(n) => {
                    let n = *n;
                    return self.call_native_ci(n, ci, this.unwrap_or(Value::Undef), sb, vb);
                }
                Value::Class(c) => {
                    return Err(self.type_error(format!("Class constructor {} cannot be invoked without 'new'", c.name)));
                }
                v => {
                    let d = describe(v);
                    return Err(self.type_error(format!("{d} is not a function")));
                }
            },
            CallTarget::Method(slot) => match this.as_ref() {
                Some(Value::Obj(o)) if o.class.is_some() => {
                    let fi = o.class.as_ref().unwrap().vtable[*slot as usize];
                    (self.prog.funcs[fi as usize].clone(), None)
                }
                Some(v) => {
                    let d = describe(v);
                    return Err(self.type_error(format!("cannot call a method on {d}")));
                }
                None => unreachable!(),
            },
            CallTarget::Prop(p) => 'prop: {
                let site = &caller.props[*p as usize];
                let recv = this.clone().unwrap_or(Value::Undef);
                // Method inline cache: same shape and class as last time → same method.
                if let Value::Obj(o) = &recv
                    && let Some(cls) = &o.class
                {
                    let hit = match &*site.cache.method.borrow() {
                        Some((sh, c, fi)) if Rc::ptr_eq(c, cls) && Rc::ptr_eq(sh, &o.inner.borrow().shape) => Some(*fi),
                        _ => None,
                    };
                    let fi = match hit {
                        Some(fi) => Some(fi),
                        None if o.inner.borrow().shape.lookup(&site.name).is_none() => {
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
                                *site.cache.method.borrow_mut() = Some((o.inner.borrow().shape.clone(), cls.clone(), fi));
                            }
                            found
                        }
                        None => None,
                    };
                    if let Some(fi) = fi {
                        break 'prop (self.prog.funcs[fi as usize].clone(), None);
                    }
                }
                match self.lookup_method(&recv, &site.name)? {
                    Value::Func(c) => (self.prog.funcs[c.func as usize].clone(), Some(c)),
                    Value::Native(n) => return self.call_native_ci(n, ci, recv, sb, vb),
                    v => {
                        let d = describe(&v);
                        return Err(self.type_error(format!("{}.{} is not a function (it is {d})", recv.type_of(), site.name)));
                    }
                }
            }
            CallTarget::Native(n) => {
                return self.call_native_ci(*n, ci, this.unwrap_or(Value::Undef), sb, vb);
            }
        };
        if self.frames.len() >= MAX_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded".into()));
        }
        let nsb = sb + caller.ns;
        let nvb = vb + caller.nv;
        self.ensure(nsb, nvb, &func);
        self.v[nvb] = this.unwrap_or(Value::Undef);
        if ci.has_spread || (func.rest && !ci.packed_rest) {
            let args = self.collect_args(ci, sb, vb)?;
            self.bind_values(&func, nsb, nvb, args)?;
        } else {
            // Fast path: copy arguments register to register, converting kinds only on mismatch.
            let (mut si, mut vi) = (0usize, 1usize);
            for (i, pk) in func.params.iter().enumerate() {
                match ci.args.get(i) {
                    Some(&(ak, ar)) => {
                        if ak == *pk {
                            match ak {
                                Kind::Val => {
                                    self.v[nvb + vi] = self.v[vb + ar as usize].clone();
                                }
                                _ => self.s[nsb + si] = self.s[sb + ar as usize],
                            }
                        } else if *pk == Kind::Val {
                            self.v[nvb + vi] = Self::boxed(ak, self.s[sb + ar as usize]);
                        } else {
                            let val = self.read_val(ak, ar, sb, vb);
                            self.s[nsb + si] = self.unbox(*pk, &val)?;
                        }
                    }
                    None => match pk {
                        Kind::Val => self.v[nvb + vi] = Value::Undef,
                        Kind::Num => self.s[nsb + si] = b(f64::NAN),
                        Kind::Bool => self.s[nsb + si] = 0,
                    },
                }
                if *pk == Kind::Val {
                    vi += 1;
                } else {
                    si += 1;
                }
            }
        }
        self.frames.push(Frame {
            func: FnRef::new(&func),
            pc: 0,
            sb: nsb,
            vb: nvb,
            closure,
            ret: match ci.dst {
                Some((k, r)) => Ret::Reg(k, r),
                None => Ret::Discard,
            },
        });
        Ok(true)
    }

    fn collect_args(&mut self, ci: &CallInfo, sb: usize, vb: usize) -> Result<Vec<Value>, Value> {
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

    fn call_native_ci(&mut self, n: Native, ci: &CallInfo, this: Value, sb: usize, vb: usize) -> Result<bool, Value> {
        let args = self.collect_args(ci, sb, vb)?;
        let r = call_native(self, n, this, args)?;
        if let Some((k, d)) = ci.dst {
            self.store(k, d, sb, vb, r)?;
        }
        Ok(false)
    }

    fn store(&mut self, k: Kind, d: R, sb: usize, vb: usize, val: Value) -> Result<(), Value> {
        match k {
            Kind::Val => self.v[vb + d as usize] = val,
            k => self.s[sb + d as usize] = self.unbox(k, &val)?,
        }
        Ok(())
    }

    /// Method lookup by name for dynamic calls.
    fn lookup_method(&mut self, recv: &Value, name: &str) -> Result<Value, Value> {
        match recv {
            Value::Obj(o) => {
                if let Some(v) = o.get(name) {
                    return Ok(v);
                }
                if let Some(c) = &o.class {
                    let mut k = Some(c.clone());
                    while let Some(c) = k {
                        if let Some(fi) = c.methods.get(name) {
                            return Ok(Value::Func(Rc::new(Closure {
                                func: *fi,
                                caps: Box::new([]),
                            })));
                        }
                        k = c.parent.clone();
                    }
                }
                Ok(Value::Undef)
            }
            Value::Undef | Value::Null => Err(self.type_error(format!(
                "Cannot read properties of {} (reading '{name}')",
                recv.to_display()
            ))),
            v => Ok(method_by_name(v, name).map_or(Value::Undef, Value::Native)),
        }
    }

    /// The getter or setter function for an accessor property, if the class has one.
    fn accessor(o: &Object, kind: &str, name: &str) -> Option<u32> {
        let c = o.class.as_ref()?;
        let key = format!("{kind} {name}");
        c.methods.get(key.as_str()).copied()
    }

    fn get_prop_slow(&mut self, recv: &Value, site: &PropSite) -> Result<Value, Value> {
        let name = &*site.name;
        match recv {
            Value::Obj(o) => {
                {
                    let inner = o.inner.borrow();
                    if let Some(i) = inner.shape.lookup(name) {
                        *site.cache.entry.borrow_mut() = Some((inner.shape.clone(), i));
                        return Ok(inner.vals[i as usize].clone());
                    }
                }
                if let Some(g) = Self::accessor(o, "get", name) {
                    let f = Value::Func(Rc::new(Closure { func: g, caps: Box::new([]) }));
                    return self.call(&f, recv.clone(), vec![]);
                }
                self.lookup_method(recv, name)
            }
            Value::Undef | Value::Null => Err(self.type_error(format!(
                "Cannot read properties of {} (reading '{name}')",
                recv.to_display()
            ))),
            Value::Arr(a) if name == "length" => Ok(Value::Num(a.len() as f64)),
            Value::Str(s) if name == "length" => Ok(Value::Num(s.len16() as f64)),
            v => {
                let r = get_dyn(v, &site.key);
                if matches!(r, Value::Undef) {
                    return Ok(method_by_name(v, name).map_or(Value::Undef, Value::Native));
                }
                Ok(r)
            }
        }
    }

    fn set_prop(&mut self, recv: &Value, site: &PropSite, val: Value) -> Result<(), Value> {
        match recv {
            Value::Obj(o) if o.inner.borrow().shape.lookup(&site.name).is_none() && Self::accessor(o, "set", &site.name).is_some() => {
                let s = Self::accessor(o, "set", &site.name).unwrap();
                let f = Value::Func(Rc::new(Closure { func: s, caps: Box::new([]) }));
                self.call(&f, recv.clone(), vec![val])?;
                Ok(())
            }
            Value::Obj(o) => {
                let mut inner = o.inner.borrow_mut();
                if let Some((sh, slot)) = &*site.cache.entry.borrow()
                    && Rc::ptr_eq(sh, &inner.shape)
                {
                    inner.vals[*slot as usize] = val;
                    return Ok(());
                }
                if let Some(i) = inner.shape.lookup(&site.name) {
                    *site.cache.entry.borrow_mut() = Some((inner.shape.clone(), i));
                    inner.vals[i as usize] = val;
                } else {
                    inner.shape = inner.shape.with_key(&site.name);
                    inner.vals.push(val);
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

    /// Pop the current frame and deliver its return value. Returns true if the
    /// run loop with base `stop` should exit.
    fn do_return(&mut self, val: Value, stop: usize) -> Result<bool, Value> {
        let fr = self.frames.pop().unwrap();
        // Release the frame's references now: deterministic destruction.
        for x in &mut self.v[fr.vb..fr.vb + fr.func.get().nv] {
            x.release();
        }
        match fr.ret {
            Ret::Reg(k, d) => {
                let top = self.frames.last().unwrap();
                let (sb, vb) = (top.sb, top.vb);
                self.store(k, d, sb, vb, val)?;
            }
            Ret::Discard => {}
            Ret::Native => self.native_ret = val,
        }
        Ok(self.frames.len() <= stop)
    }

    /// Like do_return for scalar returns without boxing in the common case.
    #[inline]
    fn do_return_s(&mut self, k: Kind, bits: u64, stop: usize) -> Result<bool, Value> {
        if let Some(fr) = self.frames.last()
            && let Ret::Reg(rk, d) = fr.ret
            && rk == k
        {
            let fr = self.frames.pop().unwrap();
            for x in &mut self.v[fr.vb..fr.vb + fr.func.get().nv] {
                x.release();
            }
            let top = self.frames.last().unwrap();
            self.s[top.sb + d as usize] = bits;
            return Ok(self.frames.len() <= stop);
        }
        self.do_return(Self::boxed(k, bits), stop)
    }

    fn unwind(&mut self, err: Value, stop: usize) -> Result<(), Value> {
        loop {
            let top = self.frames.len() - 1;
            if let Some(&(fidx, catch, exc)) = self.handlers.last()
                && fidx == top
            {
                self.handlers.pop();
                let fr = &mut self.frames[top];
                fr.pc = catch as usize;
                let vb = fr.vb;
                self.v[vb + exc as usize] = err;
                return Ok(());
            }
            let fr = self.frames.pop().unwrap();
            for x in &mut self.v[fr.vb..fr.vb + fr.func.get().nv] {
                x.release();
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
            let func: &FuncCode = self.frames[fi].func.get();
            let code = &func.code[..];
            let mut pc = self.frames[fi].pc;
            let sb = self.frames[fi].sb;
            let vb = self.frames[fi].vb;

            // Register access is unchecked: codegen guarantees every register
            // index is below the function's ns/nv, and `ensure` sized the
            // stacks before the frame was pushed. The base pointers are
            // refreshed after anything that can grow the stacks (calls).
            let mut sp: *mut u64 = unsafe { self.s.as_mut_ptr().add(sb) };
            let mut vp: *mut Value = unsafe { self.v.as_mut_ptr().add(vb) };
            macro_rules! s {
                ($r:expr) => {
                    *unsafe { &mut *sp.add($r as usize) }
                };
            }
            macro_rules! v {
                ($r:expr) => {
                    *unsafe { &mut *vp.add($r as usize) }
                };
            }
            macro_rules! n {
                ($r:expr) => {
                    f(unsafe { *sp.add($r as usize) })
                };
            }
            macro_rules! refresh {
                () => {
                    sp = unsafe { self.s.as_mut_ptr().add(sb) };
                    vp = unsafe { self.v.as_mut_ptr().add(vb) };
                };
            }
            debug_assert!(sb + func.ns <= self.s.len() && vb + func.nv <= self.v.len());
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
                match ins {
                    I::LoadN { d, v } => s!(d) = b(v),
                    I::LoadB { d, v } => s!(d) = v as u64,
                    I::MovS { d, s } => s!(d) = s!(s),
                    I::AddN { d, a, b: bb } => s!(d) = b(n!(a) + n!(bb)),
                    I::SubN { d, a, b: bb } => s!(d) = b(n!(a) - n!(bb)),
                    I::MulN { d, a, b: bb } => s!(d) = b(n!(a) * n!(bb)),
                    I::DivN { d, a, b: bb } => s!(d) = b(n!(a) / n!(bb)),
                    I::ModN { d, a, b: bb } => s!(d) = b(fast_mod(n!(a), n!(bb))),
                    I::AddNK { d, a, k } => s!(d) = b(n!(a) + k),
                    I::NumOp { op, d, a, b: bb } => s!(d) = b(num_op(op, n!(a), n!(bb))),
                    I::LtN { d, a, b: bb } => s!(d) = (n!(a) < n!(bb)) as u64,
                    I::LeN { d, a, b: bb } => s!(d) = (n!(a) <= n!(bb)) as u64,
                    I::GtN { d, a, b: bb } => s!(d) = (n!(a) > n!(bb)) as u64,
                    I::GeN { d, a, b: bb } => s!(d) = (n!(a) >= n!(bb)) as u64,
                    I::EqN { d, a, b: bb } => s!(d) = (n!(a) == n!(bb)) as u64,
                    I::NeN { d, a, b: bb } => s!(d) = (n!(a) != n!(bb)) as u64,
                    I::EqB { d, a, b: bb } => s!(d) = (s!(a) == s!(bb)) as u64,
                    I::NeB { d, a, b: bb } => s!(d) = (s!(a) != s!(bb)) as u64,
                    I::NegN { d, s } => s!(d) = b(-n!(s)),
                    I::NotB { d, s } => s!(d) = (s!(s) == 0) as u64,
                    I::BitNotN { d, s } => s!(d) = b(!to_int32(n!(s)) as f64),
                    I::TruthyN { d, s } => {
                        let x = n!(s);
                        s!(d) = (x != 0.0 && !x.is_nan()) as u64
                    }
                    I::Math1 { op, d, s } => s!(d) = b(natives::math1(op, n!(s))),
                    I::Math2 { op, d, a, b: bb } => s!(d) = b(natives::math2(op, n!(a), n!(bb))),

                    I::Jmp { t } => pc = t as usize,
                    I::JmpT { c, t } => {
                        if s!(c) != 0 {
                            pc = t as usize
                        }
                    }
                    I::JmpF { c, t } => {
                        if s!(c) == 0 {
                            pc = t as usize
                        }
                    }
                    I::JCmpN { op, sense, a, b: bb, t } => {
                        if cmp(op, n!(a), n!(bb)) == sense {
                            pc = t as usize
                        }
                    }
                    I::JmpTV { c, t } => {
                        if v!(c).truthy() {
                            pc = t as usize
                        }
                    }
                    I::JmpFV { c, t } => {
                        if !v!(c).truthy() {
                            pc = t as usize
                        }
                    }
                    I::JmpNullish { s, t } => {
                        if v!(s).is_nullish() {
                            pc = t as usize
                        }
                    }
                    I::JmpNotNullish { s, t } => {
                        if !v!(s).is_nullish() {
                            pc = t as usize
                        }
                    }

                    I::LoadK { d, k } => v!(d) = func.consts[k as usize].clone(),
                    I::LoadUndef { d } => v!(d) = Value::Undef,
                    I::LoadNull { d } => v!(d) = Value::Null,
                    I::MovV { d, s } => v!(d) = v!(s).clone(),
                    I::ClearV { d, n } => {
                        for x in &mut self.v[vb + d as usize..vb + d as usize + n as usize] {
                            x.release();
                        }
                    }
                    I::BoxN { d, s } => v!(d) = Value::Num(n!(s)),
                    I::BoxB { d, s } => v!(d) = Value::Bool(s!(s) != 0),
                    I::UnboxN { d, s } => {
                        let x = match &v!(s) {
                            Value::Num(x) => *x,
                            other => {
                                let m = format!("expected a number, got {}", describe(other));
                                break self.type_error(m);
                            }
                        };
                        s!(d) = b(x)
                    }
                    I::UnboxB { d, s } => {
                        let x = match &v!(s) {
                            Value::Bool(x) => *x,
                            other => {
                                let m = format!("expected a boolean, got {}", describe(other));
                                break self.type_error(m);
                            }
                        };
                        s!(d) = x as u64
                    }
                    I::TruthyV { d, s } => s!(d) = v!(s).truthy() as u64,
                    I::NumToStr { d, s } => v!(d) = Value::str(num_to_string(n!(s))),
                    I::ToStr { d, s } => v!(d) = Value::Str(v!(s).to_js_string()),
                    I::ToNum { d, s } => s!(d) = b(v!(s).to_number()),
                    I::Concat { d, a, b: bb } => {
                        let r = match (&v!(a), &v!(bb)) {
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
                        v!(d) = r
                    }
                    I::StrCmp { op, d, a, b: bb } => {
                        let r = match (&v!(a), &v!(bb)) {
                            (Value::Str(x), Value::Str(y)) => str_cmp(op, &x.s, &y.s),
                            (x, y) => str_cmp(op, &x.to_js_string().s, &y.to_js_string().s),
                        };
                        s!(d) = r as u64
                    }
                    I::StrictEq { d, a, b: bb, eq } => s!(d) = (v!(a).strict_eq(&v!(bb)) == eq) as u64,
                    I::LooseEq { d, a, b: bb, eq } => s!(d) = (v!(a).loose_eq(&v!(bb)) == eq) as u64,
                    I::DynAdd { d, a, b: bb } => {
                        let r = match (&v!(a), &v!(bb)) {
                            (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                            (x @ Value::Str(_), y) | (x, y @ Value::Str(_)) => {
                                Value::str(format!("{}{}", x.to_js_string().s, y.to_js_string().s))
                            }
                            (x @ (Value::Arr(_) | Value::Obj(_)), y) | (x, y @ (Value::Arr(_) | Value::Obj(_))) => {
                                Value::str(format!("{}{}", x.to_display(), y.to_display()))
                            }
                            (x, y) => Value::Num(x.to_number() + y.to_number()),
                        };
                        v!(d) = r
                    }
                    I::DynNum { op, d, a, b: bb } => s!(d) = b(dyn_num(op, &v!(a), &v!(bb))),
                    I::DynNumV { op, d, a, b: bb } => {
                        let r = dyn_num(op, &v!(a), &v!(bb));
                        v!(d) = Value::Num(r)
                    }
                    I::IncV { d, s, k } => {
                        let r = match &v!(s) {
                            Value::Num(x) => x + k,
                            x => x.to_number() + k,
                        };
                        v!(d) = Value::Num(r)
                    }
                    I::DynCmp { op, d, a, b: bb } => {
                        let r = match (&v!(a), &v!(bb)) {
                            (Value::Str(x), Value::Str(y)) => str_cmp(op, &x.s, &y.s),
                            (x, y) => cmp(op, x.to_number(), y.to_number()),
                        };
                        s!(d) = r as u64
                    }
                    I::TypeOf { d, s } => v!(d) = Value::str(v!(s).type_of()),
                    I::In { d, k, o } => {
                        let r = match &v!(o) {
                            Value::Obj(ob) => {
                                let key = v!(k).to_js_string();
                                ob.get(&key.s).is_some()
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
                            Value::Arr(a) => as_index(&v!(k)).is_some_and(|i| i < a.len()) || v!(k).to_display() == "length",
                            other => {
                                let m = format!("Cannot use 'in' operator to search for a key in {}", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        s!(d) = r as u64
                    }
                    I::InstanceOf { d, o, class } => {
                        s!(d) = matches!(&v!(o), Value::Obj(ob) if ob.class.as_ref().is_some_and(|c| c.is_subclass_of(class)))
                            as u64
                    }

                    I::NewCell { d } => v!(d) = Value::Cell(Rc::new(std::cell::RefCell::new(Value::Undef))),
                    I::CellGet { d, c } => {
                        let x = match &v!(c) {
                            Value::Cell(cell) => cell.borrow().clone(),
                            _ => unreachable!("not a cell"),
                        };
                        v!(d) = x
                    }
                    I::CellSet { c, s } => {
                        let x = v!(s).clone();
                        match &v!(c) {
                            Value::Cell(cell) => *cell.borrow_mut() = x,
                            _ => unreachable!("not a cell"),
                        }
                    }
                    I::UpGet { d, i } => {
                        let x = self.frames[fi].closure.as_ref().unwrap().caps[i as usize].borrow().clone();
                        v!(d) = x
                    }
                    I::UpSet { i, s } => {
                        let x = v!(s).clone();
                        *self.frames[fi].closure.as_ref().unwrap().caps[i as usize].borrow_mut() = x;
                    }
                    I::GetGS { d, g } => s!(d) = self.gs[g as usize],
                    I::SetGS { g, s } => self.gs[g as usize] = s!(s),
                    I::GetGV { d, g } => v!(d) = self.gv[g as usize].clone(),
                    I::SetGV { g, s } => self.gv[g as usize] = v!(s).clone(),

                    I::MakeClosure { d, f: fidx, caps } => {
                        let list = &func.caps[caps as usize];
                        let mut cells = Vec::with_capacity(list.len());
                        for c in list {
                            cells.push(match c {
                                CapSrc::Local(r) => match &v!(*r) {
                                    Value::Cell(cell) => cell.clone(),
                                    _ => unreachable!("capture source is not a cell"),
                                },
                                CapSrc::Up(i) => self.frames[fi].closure.as_ref().unwrap().caps[*i as usize].clone(),
                            });
                        }
                        v!(d) = Value::Func(Rc::new(Closure {
                            func: fidx,
                            caps: cells.into_boxed_slice(),
                        }))
                    }
                    I::Call { ci } => {
                        self.frames[fi].pc = pc;
                        let r = self.do_call(func, &func.calls[ci as usize], sb, vb);
                        refresh!();
                        match r {
                            Ok(true) => continue 'outer,
                            Ok(false) => {}
                            Err(e) => break e,
                        }
                    }
                    I::RetN { s } => {
                        if tri!(self.do_return_s(Kind::Num, s!(s), stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    I::RetB { s } => {
                        if tri!(self.do_return_s(Kind::Bool, s!(s), stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    I::RetV { s } => {
                        let x = std::mem::take(&mut v!(s));
                        if tri!(self.do_return(x, stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                    I::RetUndef => {
                        if tri!(self.do_return(Value::Undef, stop)) {
                            return Ok(());
                        }
                        continue 'outer;
                    }

                    I::NewArr { d, num } => {
                        v!(d) = Value::Arr(if num { Array::new_num(Vec::new()) } else { Array::new_val(Vec::new()) })
                    }
                    I::PushN { a, s } => {
                        let x = n!(s);
                        match &v!(a) {
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
                    I::PushV { a, s } => {
                        let x = v!(s).clone();
                        match &v!(a) {
                            Value::Arr(arr) => arr.push(x),
                            other => {
                                let m = format!("cannot push to {}", describe(other));
                                break self.type_error(m);
                            }
                        }
                    }
                    I::PushSpread { a, s } => {
                        let src = v!(s).clone();
                        let items = tri!(natives::iterate(self, &src));
                        if let Value::Arr(arr) = &v!(a) {
                            for x in items {
                                arr.push(x);
                            }
                        }
                    }
                    I::GetIdxN { d, a, i } => {
                        let idx = n!(i);
                        let x = match &v!(a) {
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
                            Ok(x) => s!(d) = b(x),
                            Err(m) => break self.type_error(m),
                        }
                    }
                    I::SetIdxN { a, i, s } => {
                        let idx = n!(i);
                        let x = n!(s);
                        match &v!(a) {
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
                    I::GetIdxV { d, a, i } => {
                        let idx = n!(i);
                        let x = match &v!(a) {
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
                        v!(d) = x
                    }
                    I::SetIdxV { a, i, s } => {
                        let idx = n!(i);
                        let x = v!(s).clone();
                        match &v!(a) {
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
                    I::GetDyn { d, o, k } => {
                        // Array element by integer index.
                        if let (Value::Arr(a), Value::Num(i)) = (&v!(o), &v!(k))
                            && *i >= 0.0
                            && (*i as usize) as f64 == *i
                        {
                            let x = a.get(*i as usize);
                            v!(d) = x;
                            continue;
                        }
                        let x = match &v!(o) {
                            other @ (Value::Undef | Value::Null) => {
                                let m = format!(
                                    "Cannot read properties of {} (reading '{}')",
                                    other.to_display(),
                                    v!(k).to_display()
                                );
                                break self.type_error(m);
                            }
                            Value::Obj(ob) => {
                                let key = v!(k).to_js_string();
                                match ob.get(&key.s) {
                                    Some(x) => x,
                                    None => tri!(self.lookup_method(&v!(o).clone(), &key.s.clone())),
                                }
                            }
                            other => {
                                let r = get_dyn(other, &v!(k));
                                if matches!(r, Value::Undef)
                                    && let Value::Str(name) = &v!(k)
                                {
                                    method_by_name(other, &name.s).map_or(Value::Undef, Value::Native)
                                } else {
                                    r
                                }
                            }
                        };
                        v!(d) = x
                    }
                    I::SetDyn { o, k, s } => {
                        let x = v!(s).clone();
                        if !set_dyn(&v!(o), &v!(k), x) {
                            let m = format!(
                                "Cannot set properties of {} (setting '{}')",
                                v!(o).to_display(),
                                v!(k).to_display()
                            );
                            break self.type_error(m);
                        }
                    }
                    I::DelDyn { o, k } => {
                        if let Value::Obj(ob) = &v!(o) {
                            let key = v!(k).to_js_string();
                            ob.delete(&key.s);
                        }
                    }
                    I::ArrLen { d, a } => {
                        let x = match &v!(a) {
                            Value::Arr(arr) => arr.len(),
                            Value::Str(st) => st.len16(),
                            other => {
                                let m = format!("Cannot read properties of {} (reading 'length')", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        s!(d) = b(x as f64)
                    }
                    I::SetArrLen { a, s } => {
                        let x = n!(s);
                        if let Value::Arr(arr) = &v!(a) {
                            if x < 0.0 || x.fract() != 0.0 {
                                break self.range_error("Invalid array length".into());
                            }
                            arr.set_len(x as usize)
                        }
                    }
                    I::StrLen { d, s } => {
                        let x = match &v!(s) {
                            Value::Str(st) => st.len16(),
                            other => {
                                let m = format!("Cannot read properties of {} (reading 'length')", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        s!(d) = b(x as f64)
                    }
                    I::CharCodeAt { d, s, i } => {
                        let idx = n!(i);
                        let x = match &v!(s) {
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
                        s!(d) = b(x)
                    }
                    I::IterArr { d, s } => {
                        let x = match &v!(s) {
                            Value::Arr(_) => v!(s).clone(),
                            other => {
                                let other = other.clone();
                                Value::Arr(Array::new_val(tri!(natives::iterate(self, &other))))
                            }
                        };
                        v!(d) = x
                    }
                    I::KeysArr { d, s } => {
                        let x = Value::Arr(Array::new_val(keys_of(&v!(s))));
                        v!(d) = x
                    }

                    I::NewObj { d, shape } => {
                        let sh = self.prog.shapes[shape as usize].clone();
                        let n = sh.keys.len();
                        let _ = n;
                        v!(d) = Value::Obj(Object::blank(sh, None))
                    }
                    I::ObjSpread { o, s } => {
                        let src = v!(s).clone();
                        let dst = v!(o).clone();
                        for k in keys_of(&src) {
                            let x = get_dyn(&src, &k);
                            set_dyn(&dst, &k, x);
                        }
                    }
                    I::GetField { d, o, i } => {
                        let x = match &v!(o) {
                            Value::Obj(ob) => ob.inner.borrow().vals[i as usize].clone(),
                            other => {
                                let m = format!("Cannot read properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        };
                        v!(d) = x
                    }
                    I::SetField { o, i, s } => {
                        let x = v!(s).clone();
                        match &v!(o) {
                            Value::Obj(ob) => ob.inner.borrow_mut().vals[i as usize] = x,
                            other => {
                                let m = format!("Cannot set properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        }
                    }
                    I::GetFieldN { d, o, i } => {
                        let x = match &v!(o) {
                            Value::Obj(ob) => match &ob.inner.borrow().vals[i as usize] {
                                Value::Num(x) => Ok(*x),
                                other => Err(format!("expected a number field, got {}", describe(other))),
                            },
                            other => Err(format!("Cannot read properties of {}", other.to_display())),
                        };
                        match x {
                            Ok(x) => s!(d) = b(x),
                            Err(m) => break self.type_error(m),
                        }
                    }
                    I::SetFieldN { o, i, s } => {
                        let x = n!(s);
                        match &v!(o) {
                            Value::Obj(ob) => ob.inner.borrow_mut().vals[i as usize] = Value::Num(x),
                            other => {
                                let m = format!("Cannot set properties of {}", other.to_display());
                                break self.type_error(m);
                            }
                        }
                    }
                    I::GetProp { d, o, p } => {
                        let site = &func.props[p as usize];
                        let fast = match &v!(o) {
                            Value::Obj(ob) => {
                                let inner = ob.inner.borrow();
                                match &*site.cache.entry.borrow() {
                                    Some((sh, slot)) if Rc::ptr_eq(sh, &inner.shape) => Some(inner.vals[*slot as usize].clone()),
                                    _ => None,
                                }
                            }
                            Value::Arr(a) if site.is_length => Some(Value::Num(a.len() as f64)),
                            _ => None,
                        };
                        let x = match fast {
                            Some(x) => x,
                            None => {
                                let recv = v!(o).clone();
                                tri!(self.get_prop_slow(&recv, site))
                            }
                        };
                        v!(d) = x
                    }
                    I::SetProp { o, p, s } => {
                        let site = &func.props[p as usize];
                        let x = v!(s).clone();
                        // Inline cache hit: the shape already has the slot.
                        let x = if let Value::Obj(ob) = &v!(o) {
                            let mut inner = ob.inner.borrow_mut();
                            match &*site.cache.entry.borrow() {
                                Some((sh, slot)) if Rc::ptr_eq(sh, &inner.shape) => {
                                    inner.vals[*slot as usize] = x;
                                    None
                                }
                                _ => Some(x),
                            }
                        } else {
                            Some(x)
                        };
                        if let Some(x) = x {
                            let recv = v!(o).clone();
                            tri!(self.set_prop(&recv, site, x));
                        }
                    }
                    I::NewInst { d, class } => {
                        let c = self.prog.classes[class as usize].clone();
                        let n = c.shape.keys.len();
                        let _ = n;
                        v!(d) = Value::Obj(Object::blank(c.shape.clone(), Some(c)))
                    }
                    I::LoadClass { d, class } => v!(d) = Value::Class(self.prog.classes[class as usize].clone()),

                    I::Throw { s } => break v!(s).clone(),
                    I::TryBegin { catch, exc } => self.handlers.push((fi, catch, exc)),
                    I::TryEnd => {
                        self.handlers.pop();
                    }
                }
            };
            // An exception was raised in this frame (or, if it already returned, in its caller).
            if fi < self.frames.len() {
                self.frames[fi].pc = pc;
            }
            self.unwind(err, stop)?;
        }
    }
}
