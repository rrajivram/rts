//! Native compilation, stage 1: numeric functions (Cranelift).
//!
//! A function whose parameters, locals and result are all numbers or
//! booleans, and whose body uses only arithmetic, comparisons, control flow,
//! `Math.*` and calls to other such functions, is compiled to machine code
//! from the typed HIR at start-up. The interpreter calls it directly (see
//! `Vm::do_call`); everything else keeps running in the interpreter.
//!
//! Number semantics are the interpreter's: `%`, `**`, ToInt32 and the `Math`
//! functions call the same Rust code. Recursion is bounded like the
//! interpreter's: each compiled function counts its depth in `JitCtx`, and
//! past `MAX_DEPTH` it sets `overflow` and returns at once, every caller
//! returning in turn, so the interpreter can throw a RangeError.

use std::cell::UnsafeCell;

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
use crate::runtime::natives::{self, Native};
use crate::types::Kind;

/// Recursion limit of compiled code (as the interpreter's).
const MAX_DEPTH: u32 = 10_000;

/// State shared by compiled code and the interpreter.
#[repr(C)]
pub struct JitCtx {
    pub depth: u32,
    pub overflow: u32,
}

struct CtxCell(UnsafeCell<JitCtx>);
// SAFETY: the VM, and so all compiled code, runs on one thread.
unsafe impl Sync for CtxCell {}
static CTX: CtxCell = CtxCell(UnsafeCell::new(JitCtx { depth: 0, overflow: 0 }));

pub fn ctx() -> *mut JitCtx {
    CTX.0.get()
}

/// Entry point of a compiled function: `(ctx, args) -> result`, with every
/// argument and the result as raw scalar-slot bits (f64 bits, or 0/1).
pub type Entry = unsafe extern "C" fn(*mut JitCtx, *const u64) -> u64;

static mut ENABLED: bool = true;

/// `--no-jit`.
pub fn disable() {
    unsafe { ENABLED = false };
}

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

/// Which functions can be compiled (a fixpoint: calls must go to compiled functions).
struct Elig<'a> {
    hir: &'a HProgram,
    set: FxHashSet<FuncId>,
}

fn scalar(k: Kind) -> bool {
    matches!(k, Kind::Num | Kind::Bool)
}

impl Elig<'_> {
    fn local(&self, f: FuncId, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(f) && !vi.captured && scalar(vi.kind())
    }

    fn func_ok(&self, f: FuncId) -> bool {
        let hf = &self.hir.funcs[f as usize];
        if hf.is_async || hf.is_generator || !hf.free.is_empty() || !matches!(hf.ret_kind, Some(Kind::Num | Kind::Bool)) {
            return false;
        }
        for p in &hf.params {
            if !scalar(p.arrival) || p.default.is_some() || p.rest || !self.local(f, p.var) || self.hir.vars[p.var as usize].kind() != p.arrival {
                return false;
            }
        }
        // Falling off the end would return undefined, which the interpreter
        // reports as a type error: only functions that always return compile.
        always_returns(&hf.body) && hf.body.iter().all(|s| self.stmt(f, s))
    }

    fn stmt(&self, f: FuncId, s: &HStmt) -> bool {
        match s {
            HStmt::Expr(e) => self.expr(f, e),
            HStmt::Init(v, e) => self.local(f, *v) && e.as_ref().is_none_or(|e| self.expr(f, e) && e.kind() == self.hir.vars[*v as usize].kind()),
            HStmt::Block(ss, vars) => vars.iter().all(|v| self.local(f, *v)) && ss.iter().all(|s| self.stmt(f, s)),
            HStmt::If(c, t, e) => self.cond(f, c) && t.iter().chain(e.iter()).all(|s| self.stmt(f, s)),
            HStmt::Loop { test, body, update, fresh, .. } => {
                fresh.is_empty()
                    && test.as_ref().is_none_or(|t| self.cond(f, t))
                    && update.as_ref().is_none_or(|u| self.expr(f, u))
                    && body.iter().all(|s| self.stmt(f, s))
            }
            HStmt::Break(_) | HStmt::Continue(_) => true,
            HStmt::Labeled(_, s) => self.stmt(f, s),
            HStmt::Return(Some(e)) => self.expr(f, e) && Some(e.kind()) == self.hir.funcs[f as usize].ret_kind,
            _ => false,
        }
    }

    fn cond(&self, f: FuncId, e: &HExpr) -> bool {
        self.expr(f, e)
    }

    fn expr(&self, f: FuncId, e: &HExpr) -> bool {
        let k = e.kind();
        if !scalar(k) {
            return false;
        }
        let num = |x: &HExpr| x.kind() == Kind::Num && self.expr(f, x);
        let boo = |x: &HExpr| x.kind() == Kind::Bool && self.expr(f, x);
        match &e.kind {
            HK::Num(_) | HK::Bool(_) => true,
            HK::Read(p) => matches!(&**p, Place::Var(v) if self.local(f, *v)),
            HK::Binary(HBin::Num(_), a, b) | HK::Binary(HBin::NumCmp(_), a, b) => num(a) && num(b),
            HK::Binary(HBin::BoolEq(_), a, b) => boo(a) && boo(b),
            HK::Unary(HUnary::NegNum | HUnary::BitNotNum, x) => num(x),
            HK::Unary(HUnary::NotBool, x) => boo(x),
            HK::Unary(HUnary::Truthy, x) => self.expr(f, x),
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => a.kind() == k && b.kind() == k && self.expr(f, a) && self.expr(f, b),
            HK::Cond(c, a, b) => self.cond(f, c) && a.kind() == k && b.kind() == k && self.expr(f, a) && self.expr(f, b),
            HK::Assign(p, x) => matches!(&**p, Place::Var(v) if self.local(f, *v) && self.hir.vars[*v as usize].kind() == x.kind() && x.kind() == k) && self.expr(f, x),
            HK::Compound(p, HBin::Num(_), x) => matches!(&**p, Place::Var(v) if self.local(f, *v) && self.hir.vars[*v as usize].kind() == Kind::Num) && num(x),
            HK::Update { place, .. } => matches!(&**place, Place::Var(v) if self.local(f, *v) && self.hir.vars[*v as usize].kind() == Kind::Num),
            HK::Cast(x) => x.kind() == k && self.expr(f, x),
            HK::Seq(xs) => xs.iter().all(|x| self.expr(f, x)),
            HK::Call(c) => self.call(f, c, k),
            _ => false,
        }
    }

    fn call(&self, f: FuncId, c: &HCall, k: Kind) -> bool {
        if c.opt || c.args.iter().any(|a| matches!(a, HElem::Spread(_))) {
            return false;
        }
        let args: Vec<&HExpr> = c.args.iter().map(|a| if let HElem::Expr(x) = a { x } else { unreachable!() }).collect();
        match &c.callee {
            Callee::Direct { func, closure: None } if self.set.contains(func) && c.this.is_none() => {
                let hf = &self.hir.funcs[*func as usize];
                hf.ret_kind == Some(k)
                    && hf.params.len() == args.len()
                    && hf.params.iter().zip(&args).all(|(p, a)| p.arrival == a.kind() && self.expr(f, a))
            }
            Callee::Native(n) if k == Kind::Num && args.iter().all(|a| a.kind() == Kind::Num && self.expr(f, a)) => {
                let one = matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(n);
                (one && args.len() == 1) || (MATH2.contains(n) && args.len() == 2)
            }
            _ => false,
        }
    }
}

/// Every path through `ss` ends in a `return`.
fn always_returns(ss: &[HStmt]) -> bool {
    match ss.last() {
        Some(HStmt::Return(_)) => true,
        Some(HStmt::Block(b, _)) => always_returns(b),
        Some(HStmt::If(_, t, e)) => always_returns(t) && always_returns(e),
        Some(HStmt::Labeled(_, s)) => always_returns(std::slice::from_ref(&**s)),
        _ => false,
    }
}

fn cl_ty(k: Kind) -> types::Type {
    if k == Kind::Num { types::F64 } else { types::I8 }
}

struct Helpers {
    modf: FuncRef,
    pow: FuncRef,
    int32: FuncRef,
    math1: FuncRef,
    math2: FuncRef,
}

struct Lower<'a, 'b> {
    hir: &'a HProgram,
    fid: FuncId,
    b: FunctionBuilder<'b>,
    vars: FxHashMap<VarId, Variable>,
    ctx: CV,
    depth: CV,
    /// (label, break target, continue target).
    loops: Vec<(Option<Name>, Block, Option<Block>)>,
    funcs: FxHashMap<FuncId, FuncRef>,
    h: Helpers,
}

impl Lower<'_, '_> {
    fn var(&mut self, v: VarId) -> Variable {
        if let Some(x) = self.vars.get(&v) {
            return *x;
        }
        let k = self.hir.vars[v as usize].kind();
        let x = self.b.declare_var(cl_ty(k));
        let init = if k == Kind::Num { self.b.ins().f64const(f64::NAN) } else { self.b.ins().iconst(types::I8, 0) };
        self.b.def_var(x, init);
        self.vars.insert(v, x);
        x
    }

    /// Code after a jump, return or break is unreachable: give it a block of its own.
    fn dead_block(&mut self) {
        let blk = self.b.create_block();
        self.b.switch_to_block(blk);
        self.b.seal_block(blk);
    }

    fn ret(&mut self, v: CV) {
        self.b.ins().store(MemFlagsData::trusted(), self.depth, self.ctx, 0);
        self.b.ins().return_(&[v]);
    }

    fn zero(&mut self) -> CV {
        match self.hir.funcs[self.fid as usize].ret_kind {
            Some(Kind::Num) => self.b.ins().f64const(f64::NAN),
            _ => self.b.ins().iconst(types::I8, 0),
        }
    }

    fn stmts(&mut self, ss: &[HStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &HStmt) {
        match s {
            HStmt::Expr(e) => {
                self.expr(e);
            }
            HStmt::Init(v, e) => {
                let x = self.var(*v);
                let val = match e {
                    Some(e) => self.expr(e),
                    None if self.hir.vars[*v as usize].kind() == Kind::Num => self.b.ins().f64const(f64::NAN),
                    None => self.b.ins().iconst(types::I8, 0),
                };
                self.b.def_var(x, val);
            }
            HStmt::Block(ss, vars) => {
                for v in vars {
                    self.var(*v);
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
                    self.expr(u);
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
            HStmt::Return(Some(e)) => {
                let v = self.expr(e);
                self.ret(v);
                self.dead_block();
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// A condition as an i8 (0/1): booleans as they are, numbers by truthiness.
    fn truthy(&mut self, e: &HExpr) -> CV {
        let v = self.expr(e);
        self.truth(v, e.kind())
    }

    fn truth(&mut self, v: CV, k: Kind) -> CV {
        if k == Kind::Bool {
            return v;
        }
        let z = self.b.ins().f64const(0.0);
        self.b.ins().fcmp(FloatCC::OrderedNotEqual, v, z)
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

    fn expr(&mut self, e: &HExpr) -> CV {
        let k = e.kind();
        match &e.kind {
            HK::Num(x) => self.b.ins().f64const(*x),
            HK::Bool(x) => self.b.ins().iconst(types::I8, *x as i64),
            HK::Read(p) => {
                let Place::Var(v) = &**p else { unreachable!() };
                let x = self.var(*v);
                self.b.use_var(x)
            }
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
            HK::Assign(p, x) => {
                let Place::Var(v) = &**p else { unreachable!() };
                let val = self.expr(x);
                let var = self.var(*v);
                self.b.def_var(var, val);
                val
            }
            HK::Compound(p, HBin::Num(op), x) => {
                let Place::Var(v) = &**p else { unreachable!() };
                let var = self.var(*v);
                let cur = self.b.use_var(var);
                let y = self.expr(x);
                let r = self.num_op(*op, cur, y);
                self.b.def_var(var, r);
                r
            }
            HK::Update { place, inc, prefix } => {
                let Place::Var(v) = &**place else { unreachable!() };
                let var = self.var(*v);
                let cur = self.b.use_var(var);
                let one = self.b.ins().f64const(1.0);
                let r = if *inc { self.b.ins().fadd(cur, one) } else { self.b.ins().fsub(cur, one) };
                self.b.def_var(var, r);
                if *prefix { r } else { cur }
            }
            HK::Cast(x) => self.expr(x),
            HK::Seq(xs) => {
                let mut last = None;
                for x in xs {
                    last = Some(self.expr(x));
                }
                last.unwrap_or_else(|| self.b.ins().f64const(f64::NAN))
            }
            HK::Call(c) => self.call(c),
            _ => unreachable!("checked by Elig"),
        }
    }

    fn call(&mut self, c: &HCall) -> CV {
        let args: Vec<CV> = c.args.iter().map(|a| if let HElem::Expr(x) = a { self.expr(x) } else { unreachable!() }).collect();
        match &c.callee {
            Callee::Direct { func, .. } => {
                let fr = self.funcs[func];
                let mut all = vec![self.ctx];
                all.extend(args);
                let call = self.b.ins().call(fr, &all);
                let r = self.b.inst_results(call)[0];
                // A callee that ran out of stack: return at once, all the way out.
                let flag = self.b.ins().load(types::I32, MemFlagsData::trusted(), self.ctx, 4);
                let (bail, ok) = (self.b.create_block(), self.b.create_block());
                self.b.ins().brif(flag, bail, &[], ok, &[]);
                self.b.seal_block(bail);
                self.b.seal_block(ok);
                self.b.switch_to_block(bail);
                let z = self.zero();
                self.ret(z);
                self.b.switch_to_block(ok);
                r
            }
            Callee::Native(n) => {
                let ins = self.b.ins();
                match n {
                    Native::MathFloor => ins.floor(args[0]),
                    Native::MathCeil => ins.ceil(args[0]),
                    Native::MathTrunc => ins.trunc(args[0]),
                    Native::MathSqrt => ins.sqrt(args[0]),
                    Native::MathAbs => ins.fabs(args[0]),
                    _ => {
                        let (h, i, rest) = if let Some(i) = MATH1.iter().position(|m| m == n) {
                            (self.h.math1, i, &args[..1])
                        } else {
                            (self.h.math2, MATH2.iter().position(|m| m == n).unwrap(), &args[..2])
                        };
                        let idx = self.b.ins().iconst(types::I32, i as i64);
                        let mut all = vec![idx];
                        all.extend_from_slice(rest);
                        let call = self.b.ins().call(h, &all);
                        self.b.inst_results(call)[0]
                    }
                }
            }
            _ => unreachable!("checked by Elig"),
        }
    }
}

/// Compile every eligible function and attach its entry point to `prog`.
/// Returns how many were compiled.
pub fn compile(hir: &HProgram, prog: &mut Program) -> usize {
    if !unsafe { ENABLED } {
        return 0;
    }
    // Eligible functions: a fixpoint, since a call must go to a compiled function.
    let mut el = Elig { hir, set: (0..hir.funcs.len() as FuncId).collect() };
    loop {
        let keep: FxHashSet<FuncId> = el.set.iter().copied().filter(|&f| el.func_ok(f)).collect();
        if keep.len() == el.set.len() {
            break;
        }
        el.set = keep;
    }
    if el.set.is_empty() {
        return 0;
    }
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    let Ok(isa) = cranelift_native::builder().and_then(|b| b.finish(settings::Flags::new(flags)).map_err(|_| "isa")) else {
        return 0;
    };
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("rts_mod", h_mod as *const u8);
    jb.symbol("rts_pow", h_pow as *const u8);
    jb.symbol("rts_int32", h_int32 as *const u8);
    jb.symbol("rts_math1", h_math1 as *const u8);
    jb.symbol("rts_math2", h_math2 as *const u8);
    let mut m = JITModule::new(jb);
    let ptr = m.target_config().pointer_type();

    let sig = |m: &JITModule, params: &[types::Type], ret: types::Type| -> Signature {
        let mut s = m.make_signature();
        s.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        s.returns.push(AbiParam::new(ret));
        s
    };
    let h_mod_id = m.declare_function("rts_mod", Linkage::Import, &sig(&m, &[types::F64, types::F64], types::F64)).unwrap();
    let h_pow_id = m.declare_function("rts_pow", Linkage::Import, &sig(&m, &[types::F64, types::F64], types::F64)).unwrap();
    let h_int_id = m.declare_function("rts_int32", Linkage::Import, &sig(&m, &[types::F64], types::I32)).unwrap();
    let h_m1_id = m.declare_function("rts_math1", Linkage::Import, &sig(&m, &[types::I32, types::F64], types::F64)).unwrap();
    let h_m2_id = m.declare_function("rts_math2", Linkage::Import, &sig(&m, &[types::I32, types::F64, types::F64], types::F64)).unwrap();

    // Declare every compiled function (typed) and its entry trampoline.
    let mut ids: Vec<(FuncId, ClFuncId, ClFuncId)> = Vec::new();
    let mut fsigs: FxHashMap<FuncId, Signature> = FxHashMap::default();
    let mut order: Vec<FuncId> = el.set.iter().copied().collect();
    order.sort();
    for &f in &order {
        let hf = &hir.funcs[f as usize];
        let mut params = vec![ptr];
        params.extend(hf.params.iter().map(|p| cl_ty(p.arrival)));
        let s = sig(&m, &params, cl_ty(hf.ret_kind.unwrap()));
        let id = m.declare_function(&format!("f{f}"), Linkage::Local, &s).unwrap();
        let eid = m.declare_function(&format!("e{f}"), Linkage::Local, &sig(&m, &[ptr, ptr], types::I64)).unwrap();
        fsigs.insert(f, s);
        ids.push((f, id, eid));
    }
    let id_of: FxHashMap<FuncId, ClFuncId> = ids.iter().map(|(f, id, _)| (*f, *id)).collect();

    let mut ctx = m.make_context();
    let mut fbc = FunctionBuilderContext::new();
    for &(f, id, eid) in &ids {
        // The function itself.
        ctx.func.signature = fsigs[&f].clone();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let mut funcs = FxHashMap::default();
            for (g, gid) in &id_of {
                funcs.insert(*g, m.declare_func_in_func(*gid, b.func));
            }
            let h = Helpers {
                modf: m.declare_func_in_func(h_mod_id, b.func),
                pow: m.declare_func_in_func(h_pow_id, b.func),
                int32: m.declare_func_in_func(h_int_id, b.func),
                math1: m.declare_func_in_func(h_m1_id, b.func),
                math2: m.declare_func_in_func(h_m2_id, b.func),
            };
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let params: Vec<CV> = b.block_params(entry).to_vec();
            let c = params[0];
            // Depth check.
            let depth = b.ins().load(types::I32, MemFlagsData::trusted(), c, 0);
            let d1 = b.ins().iadd_imm_s(depth, 1);
            b.ins().store(MemFlagsData::trusted(), d1, c, 0);
            let over = b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, d1, MAX_DEPTH as i64);
            let (bail, body) = (b.create_block(), b.create_block());
            b.ins().brif(over, bail, &[], body, &[]);
            b.seal_block(bail);
            b.seal_block(body);
            b.switch_to_block(bail);
            let one = b.ins().iconst(types::I32, 1);
            b.ins().store(MemFlagsData::trusted(), one, c, 4);
            b.ins().store(MemFlagsData::trusted(), depth, c, 0);
            let rk = hir.funcs[f as usize].ret_kind.unwrap();
            let z = if rk == Kind::Num { b.ins().f64const(f64::NAN) } else { b.ins().iconst(types::I8, 0) };
            b.ins().return_(&[z]);
            b.switch_to_block(body);
            let mut lw = Lower { hir, fid: f, b, vars: FxHashMap::default(), ctx: c, depth, loops: vec![], funcs, h };
            let hf = &hir.funcs[f as usize];
            for (p, v) in hf.params.iter().zip(&params[1..]) {
                let x = lw.var(p.var);
                lw.b.def_var(x, *v);
            }
            lw.stmts(&hf.body);
            // Falling off the end returns undefined: NaN or false.
            let z = lw.zero();
            lw.ret(z);
            lw.b.finalize(m.isa().frontend_config());
        }
        if let Err(e) = m.define_function(id, &mut ctx) {
            if std::env::var_os("RTS_JIT_DEBUG").is_some() {
                eprintln!("rts jit: {}: {e:?}", hir.funcs[f as usize].name);
            }
            return 0;
        }
        m.clear_context(&mut ctx);

        // The entry trampoline: unpack argument bits, call, pack the result.
        ctx.func.signature = sig(&m, &[ptr, ptr], types::I64);
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let fr = m.declare_func_in_func(id, b.func);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let (c, argv) = (b.block_params(blk)[0], b.block_params(blk)[1]);
            let mut args = vec![c];
            for (i, p) in hir.funcs[f as usize].params.iter().enumerate() {
                let bits = b.ins().load(types::I64, MemFlagsData::trusted(), argv, (i * 8) as i32);
                args.push(if p.arrival == Kind::Num {
                    b.ins().bitcast(types::F64, MemFlagsData::new(), bits)
                } else {
                    b.ins().ireduce(types::I8, bits)
                });
            }
            let call = b.ins().call(fr, &args);
            let r = b.inst_results(call)[0];
            let out = if hir.funcs[f as usize].ret_kind == Some(Kind::Num) {
                b.ins().bitcast(types::I64, MemFlagsData::new(), r)
            } else {
                b.ins().uextend(types::I64, r)
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
    for &(f, _, eid) in &ids {
        let p = m.get_finalized_function(eid);
        if let Some(fc) = prog.funcs.get_mut(f as usize).and_then(std::rc::Rc::get_mut) {
            fc.jit = Some((unsafe { std::mem::transmute::<*const u8, Entry>(p) }, hir.funcs[f as usize].ret_kind.unwrap()));
        }
    }
    // The code must outlive the program.
    std::mem::forget(m);
    ids.len()
}
