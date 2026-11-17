//! Which functions compile (`Elig`), why others don't, and walks over the HIR.

use super::*;

// ───────────── eligibility ─────────────

/// Which kind of reference a value of type `t` is, if compiled code can
/// hold it (a class instance or an array, never both, or null/undefined).
pub(super) fn ref_class(t: &Type) -> Option<JV> {
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
pub(super) fn jv(t: &Type) -> Option<JV> {
    match t.kind() {
        Kind::Num => Some(JV::Num),
        Kind::Bool => Some(JV::Bool),
        Kind::Big => None,
        Kind::Val => Some(ref_class(t).unwrap_or(JV::Val)),
    }
}

/// Can compiled code convert a `from` to a `to` (as codegen's `coerce`
/// does: boxing, or unboxing with a check)?
pub(super) fn coercible(from: JK, to: JK) -> bool {
    from == to || from == JK::Val || to == JK::Val
}

pub(super) fn jk(t: &Type) -> Option<JK> {
    jv(t).map(JV::jk)
}

/// A function's result, in detail.
pub(super) fn ret_jv(hf: &HFunc) -> Option<JV> {
    match hf.ret_kind {
        None => Some(JV::Nil),
        Some(Kind::Num) => Some(JV::Num),
        Some(Kind::Bool) => Some(JV::Bool),
        Some(Kind::Val) => Some(hf.ret_ty.as_ref().and_then(ref_class).unwrap_or(JV::Val)),
        Some(Kind::Big) => None,
    }
}

/// A function's result, as compiled code returns it (`void` is `undefined`).
pub(super) fn ret_jk(hf: &HFunc) -> Option<JK> {
    match hf.ret_kind {
        None => Some(JK::Ref),
        k => ret_jv(hf).filter(|_| k.is_some()).map(JV::jk),
    }
}

/// Which functions can be compiled (a fixpoint: calls must go to compiled functions).
pub(super) struct Elig<'a> {
    pub(super) hir: &'a HProgram,
    pub(super) set: FxHashSet<FuncId>,
    pub(super) pure: FxHashSet<FuncId>,
    /// Each class's subclasses (itself included).
    pub(super) subclasses: Vec<Vec<u32>>,
}

impl Elig<'_> {
    pub(super) fn local(&self, f: FuncId, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(f) && !vi.captured && jk(&vi.ty).is_some()
    }

    /// `f`'s own variable that a closure captured: it lives in a cell.
    pub(super) fn cell(&self, f: FuncId, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(f) && vi.captured && jk(&vi.ty).is_some()
    }

    /// An enclosing function's variable `f` uses (through its closure's cells).
    pub(super) fn free(&self, f: FuncId, v: VarId) -> bool {
        self.hir.funcs[f as usize].free.contains(&v) && jk(&self.hir.vars[v as usize].ty).is_some()
    }

    /// A variable compiled code can read and write, wherever it lives.
    pub(super) fn var_ok(&self, f: FuncId, v: VarId) -> bool {
        self.local(f, v) || self.cell(f, v) || self.free(f, v) || self.global(v)
    }

    /// A variable `f` declares (in a register or a cell).
    pub(super) fn own_var(&self, f: FuncId, v: VarId) -> bool {
        self.local(f, v) || self.cell(f, v)
    }

    /// `f` can make a closure of `g`: each cell `g` captures is one of
    /// `f`'s or one `f` captured itself.
    pub(super) fn closure_ok(&self, f: FuncId, g: FuncId) -> bool {
        let hf = &self.hir.funcs[f as usize];
        self.hir.funcs[g as usize].free.iter().all(|v| {
            let vi = &self.hir.vars[*v as usize];
            (vi.owner == Some(f) && vi.captured) || hf.free.contains(v)
        })
    }

    /// A global variable compiled code can read and write.
    pub(super) fn global(&self, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner.is_none() && jk(&vi.ty).is_some()
    }

    pub(super) fn vjk(&self, v: VarId) -> Option<JK> {
        jk(&self.hir.vars[v as usize].ty)
    }

    /// The function a call reaches, if known (devirtualized).
    pub(super) fn target(&self, c: &HCall) -> Option<FuncId> {
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
    pub(super) fn effects(&self, e: &HExpr, assigns_only: bool) -> bool {
        let top = match &e.kind {
            HK::Assign(..) | HK::Compound(..) | HK::Update { .. } | HK::LogicalAssign(..) => true,
            HK::Call(c) => !assigns_only && !self.pure_call(c),
            // These may run any code (a getter, `toString`, a constructor).
            HK::New { .. } | HK::Array(_) | HK::NewBuiltin(..) | HK::Unary(HUnary::ToStr, _) | HK::Binary(HBin::DynAdd, ..) => !assigns_only,
            HK::Read(p) => !assigns_only && matches!(**p, Place::Prop { .. } | Place::DynIndex { .. }),
            _ => false,
        };
        top || children(e).into_iter().any(|x| self.effects(x, assigns_only))
    }

    /// A call that changes nothing: `Math`, or a pure function.
    pub(super) fn pure_call(&self, c: &HCall) -> bool {
        match c.callee {
            Callee::Native(n) => math_native(n),
            _ => self.target(c).is_some_and(|g| self.pure.contains(&g)),
        }
    }

    /// Pure functions write no field and call only pure functions: while one
    /// runs, nothing reachable from its arguments can change or be freed.
    pub(super) fn compute_pure(&mut self) {
        self.pure = self.set.clone();
        loop {
            let keep: FxHashSet<FuncId> = self.pure.iter().copied().filter(|&f| self.pure_stmts(&self.hir.funcs[f as usize].body)).collect();
            if keep.len() == self.pure.len() {
                break;
            }
            self.pure = keep;
        }
    }

    pub(super) fn pure_stmts(&self, ss: &[HStmt]) -> bool {
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

    pub(super) fn pure_expr(&self, e: &HExpr) -> bool {
        let top = match &e.kind {
            HK::Assign(p, _) | HK::Compound(p, _, _) | HK::Update { place: p, .. } => {
                matches!(**p, Place::Var(v) if self.hir.vars[v as usize].owner.is_some() && !self.hir.vars[v as usize].captured)
            }
            HK::New { .. } | HK::Array(_) | HK::NewBuiltin(..) | HK::Unary(HUnary::ToStr, _) | HK::Binary(HBin::DynAdd, ..) => false,
            HK::Read(p) => !matches!(**p, Place::Prop { .. } | Place::DynIndex { .. }),
            HK::Call(c) => self.pure_call(c),
            _ => true,
        };
        top && children(e).into_iter().all(|x| self.pure_expr(x))
    }

    /// In a pure function: the reference variables that need no counts.
    /// Each holds only values reachable from the arguments (a parameter's,
    /// another such variable's, a field of one, null): nothing can free them
    /// while the function runs.
    pub(super) fn borrowed_vars(&self, f: FuncId, vars: &[VarId]) -> FxHashSet<VarId> {
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
    pub(super) fn devirt(&self, class: u32, vslot: u32) -> Option<FuncId> {
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

    pub(super) fn func_ok(&self, f: FuncId) -> bool {
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
    pub(super) fn why(&self, f: FuncId, ss: &[HStmt]) {
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

    pub(super) fn why_expr(&self, f: FuncId, e: &HExpr) {
        let subs = children(e);
        for x in subs {
            if self.expr(f, x).is_none() {
                return self.why_expr(f, x);
            }
        }
        why_say(format!("({:?}): {:?}", e.ty, e.kind));
    }

    pub(super) fn stmt(&self, f: FuncId, s: &HStmt) -> bool {
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
    pub(super) fn expr(&self, f: FuncId, e: &HExpr) -> Option<JK> {
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
            HK::Compound(p, op, x) => compound_assign(self.hir, p, *op, x, &e.ty).is_some_and(|a| self.expr(f, &a) == Some(k)),
            // Untyped operators: through the interpreter's implementation.
            HK::Binary(HBin::DynAdd | HBin::DynNumeric(_), a, b) => k == JK::Val && fits(a, JK::Val) && fits(b, JK::Val),
            HK::Binary(HBin::DynNum(_), a, b) => k == JK::Num && fits(a, JK::Val) && fits(b, JK::Val),
            HK::Binary(HBin::DynCmp(_), a, b) => k == JK::Bool && fits(a, JK::Val) && fits(b, JK::Val),
            HK::Unary(HUnary::NegDyn | HUnary::BitNotDyn, x) => k == JK::Val && fits(x, JK::Val),
            HK::Unary(HUnary::ToNum, x) => k == JK::Num && fits(x, JK::Val),
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
    pub(super) fn new_direct(&self, f: FuncId, class: u32, args: &[HElem]) -> bool {
        let ctor = self.hir.classes[class as usize].ctor;
        let hc = &self.hir.funcs[ctor as usize];
        self.set.contains(&ctor) && hc.this_var.is_some() && hc.free.is_empty() && self.args(f, ctor, args)
    }

    /// Can compiled code evaluate `x` and convert it to a `want`?
    pub(super) fn fits(&self, f: FuncId, x: &HExpr, want: JK) -> bool {
        self.expr(f, x).is_some_and(|k| coercible(k, want))
    }

    /// Arguments passed as values (to the interpreter).
    pub(super) fn val_args(&self, f: FuncId, args: &[HElem]) -> bool {
        args.iter().all(|a| matches!(a, HElem::Expr(x) if self.fits(f, x, JK::Val)))
    }

    /// An expression used only for its truth: `&&`/`||` may mix kinds.
    pub(super) fn cond(&self, f: FuncId, e: &HExpr) -> bool {
        match &e.kind {
            HK::Logical(LogicalOp::And | LogicalOp::Or, a, b) => self.cond(f, a) && self.cond(f, b),
            HK::Unary(HUnary::Truthy, x) => self.cond(f, x),
            _ => self.expr(f, e).is_some(),
        }
    }

    /// A place compiled code can read as `k`. A field or element holds any
    /// value, so reads check its tag; a variable's value is converted.
    pub(super) fn place(&self, f: FuncId, p: &Place, k: JK) -> bool {
        match p {
            Place::Var(v) => self.var_ok(f, *v) && coercible(self.vjk(*v).unwrap(), k),
            Place::Field { obj, opt: false, .. } => self.expr(f, obj) == Some(JK::Ref) && jv(&obj.ty) == Some(JV::Obj),
            Place::Index { obj, index, opt: false } => self.is_array(f, obj) && self.expr(f, index) == Some(JK::Num),
            Place::Prop { obj, opt: false, .. } => self.fits(f, obj, JK::Val),
            Place::DynIndex { obj, key, opt: false } => self.fits(f, obj, JK::Val) && self.fits(f, key, JK::Val),
            _ => false,
        }
    }

    /// A place holding a number, for `+=` and `++`.
    pub(super) fn num_place(&self, f: FuncId, p: &Place) -> bool {
        match p {
            Place::Var(v) => self.var_ok(f, *v) && self.vjk(*v) == Some(JK::Num),
            Place::Field { .. } | Place::Index { .. } => self.place(f, p, JK::Num),
            _ => false,
        }
    }

    /// `p = x`, its result a `k`.
    pub(super) fn assign_ok(&self, f: FuncId, p: &Place, x: &HExpr, k: JK) -> bool {
        let Some(xk) = self.expr(f, x) else { return false };
        let store = match p {
            Place::Var(v) => self.vjk(*v).unwrap(),
            _ => xk,
        };
        self.place(f, p, store) && coercible(xk, store) && coercible(store, k)
    }

    pub(super) fn is_array(&self, f: FuncId, e: &HExpr) -> bool {
        jv(&e.ty) == Some(JV::Arr) && self.expr(f, e).is_some()
    }

    pub(super) fn args(&self, f: FuncId, callee: FuncId, args: &[HElem]) -> bool {
        let hf = &self.hir.funcs[callee as usize];
        hf.params.len() == args.len()
            && hf.params.iter().zip(args).all(|(p, a)| match a {
                HElem::Expr(x) => self.fits(f, x, self.vjk(p.var).unwrap()),
                HElem::Spread(_) => false,
            })
    }

    /// A method call with several implementations, all compiled and taking
    /// the same kinds: each receiver class (the static class and its
    /// subclasses) and the function it calls. Compiled code dispatches on
    /// the receiver's class and calls the implementation directly.
    pub(super) fn virt_targets(&self, f: FuncId, c: &HCall) -> Option<Vec<(u32, FuncId)>> {
        let Callee::Method { class, vslot } = c.callee else { return None };
        let t = c.this.as_ref()?;
        if c.opt || self.devirt(class, vslot).is_some() {
            return None;
        }
        let sig = |g: FuncId| -> Option<(Vec<Option<JK>>, Option<JV>)> {
            let hg = &self.hir.funcs[g as usize];
            if !self.set.contains(&g) || !hg.free.is_empty() || hg.params.len() != c.args.len() {
                return None;
            }
            let tv = hg.this_var?;
            if var_jv(self.hir, tv) != JV::Obj {
                return None;
            }
            let kinds = std::iter::once(tv).chain(hg.params.iter().map(|p| p.var)).map(|v| self.vjk(v)).collect();
            Some((kinds, ret_jv(hg)))
        };
        let mut out = Vec::new();
        let mut first = None;
        for &k in &self.subclasses[class as usize] {
            let g = self.hir.classes[k as usize].vtable.get(vslot as usize)?.1;
            let s = sig(g)?;
            match &first {
                None => first = Some(s),
                Some(f0) if *f0 == s => {}
                Some(_) => return None,
            }
            out.push((k, g));
        }
        let g0 = out.first()?.1;
        (self.fits(f, t, JK::Ref) && self.args(f, g0, &c.args)).then_some(out)
    }

    /// The compiled function a call goes to directly, if any.
    pub(super) fn direct_target(&self, c: &HCall) -> Option<FuncId> {
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
    pub(super) fn inline_push(&self, f: FuncId, c: &HCall, k: JK) -> bool {
        matches!(c.callee, Callee::Native(Native::ArrPush))
            && k == JK::Num
            && c.this.as_ref().is_some_and(|t| self.is_array(f, t))
            && matches!(&c.args[..], [HElem::Expr(x)] if self.expr(f, x).is_some())
    }

    /// A `Math` call compiled inline: numbers in, a number out.
    pub(super) fn inline_math(&self, f: FuncId, c: &HCall, n: Native, k: JK) -> bool {
        let one = matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(&n);
        let arity_ok = (one && c.args.len() == 1) || (MATH2.contains(&n) && c.args.len() == 2);
        c.this.is_none() && k == JK::Num && arity_ok && c.args.iter().all(|a| matches!(a, HElem::Expr(x) if self.expr(f, x) == Some(JK::Num)))
    }

    pub(super) fn call(&self, f: FuncId, c: &HCall, k: JK) -> bool {
        if c.opt {
            return false;
        }
        if let Some(g) = self.direct_target(c) {
            let hg = &self.hir.funcs[g as usize];
            let this_ok = c.this.as_ref().is_none_or(|t| hg.this_var.is_some_and(|tv| self.fits(f, t, self.vjk(tv).unwrap())));
            return this_ok && coercible(ret_jk(hg).unwrap(), k) && self.args(f, g, &c.args);
        }
        if let Some(t) = self.virt_targets(f, c) {
            return coercible(ret_jk(&self.hir.funcs[t[0].1 as usize]).unwrap(), k);
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
/// `p op= x` as `p = p op x`, for a string `+=` or an untyped operator,
/// when evaluating the place twice is safe (its object and key are
/// variables, fields of variables, or literals).
pub(super) fn compound_assign(hir: &HProgram, p: &Place, op: HBin, x: &HExpr, ty: &Type) -> Option<HExpr> {
    fn plain(e: &HExpr) -> bool {
        match &e.kind {
            HK::Read(p) => match &**p {
                Place::Var(_) => true,
                Place::Field { obj, opt: false, .. } => plain(obj),
                _ => false,
            },
            HK::Cast(y) => plain(y),
            HK::Num(_) | HK::Str(_) => true,
            _ => false,
        }
    }
    // The current value's type: the variable's, or the dynamic place's (any).
    let cur_ty = match (op, p) {
        (HBin::Concat, Place::Var(_)) => ty.clone(),
        (HBin::Concat, Place::Field { obj, opt: false, .. }) if plain(obj) => ty.clone(),
        (HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_), Place::Var(v)) => hir.vars[*v as usize].ty.clone(),
        (HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_), Place::Prop { obj, opt: false, .. }) if plain(obj) => Type::Any,
        (HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_), Place::DynIndex { obj, key, opt: false }) if plain(obj) && plain(key) => Type::Any,
        _ => return None,
    };
    let res_ty = match op {
        HBin::Concat => ty.clone(),
        HBin::DynNum(_) => Type::Number,
        _ => Type::Any,
    };
    let cur = HExpr { kind: HK::Read(Box::new(p.clone())), ty: cur_ty };
    let res = HExpr { kind: HK::Binary(op, Box::new(cur), Box::new(x.clone())), ty: res_ty };
    Some(HExpr { kind: HK::Assign(Box::new(p.clone()), Box::new(res)), ty: ty.clone() })
}

/// `throw new C("…", …)` with string literal arguments: the class and arguments.
pub(super) fn throw_site(e: &HExpr) -> Option<(u32, Vec<std::rc::Rc<str>>)> {
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
    pub(super) static WHY_FN: RefCell<String> = const { RefCell::new(String::new()) };
}

/// `RTS_JIT_WHY=*` explains every function; `RTS_JIT_WHY=name` one.
pub(super) fn why_all() -> bool {
    std::env::var("RTS_JIT_WHY").is_ok_and(|n| n == "*")
}

pub(super) fn why_wanted(name: &str) -> bool {
    std::env::var("RTS_JIT_WHY").is_ok_and(|n| n == "*" || n == name)
}

pub(super) fn why_say(msg: String) {
    let f = WHY_FN.with(|w| w.borrow().clone());
    if why_all() {
        let m: String = msg.chars().take(110).collect();
        eprintln!("rts jit why\t{f}\t{m}");
    } else {
        eprintln!("rts jit: {f} not compiled: {msg}");
    }
}

/// Every path through `ss` ends in a `return`.
pub(super) fn always_returns(ss: &[HStmt]) -> bool {
    match ss.last() {
        Some(HStmt::Return(_) | HStmt::Throw(_)) => true,
        Some(HStmt::Block(b, _)) => always_returns(b),
        Some(HStmt::If(_, t, e)) => always_returns(t) && always_returns(e),
        Some(HStmt::Labeled(_, s)) => always_returns(std::slice::from_ref(&**s)),
        _ => false,
    }
}

/// The expressions inside a place (its object, index).
pub(super) fn place_children(p: &Place) -> Vec<&HExpr> {
    match p {
        Place::Var(_) => vec![],
        Place::Field { obj, .. } | Place::Prop { obj, .. } | Place::ArrLen { obj, .. } | Place::StrLen { obj, .. } | Place::Accessor { obj, .. } => vec![obj],
        Place::Index { obj, index, .. } => vec![obj, index],
        Place::DynIndex { obj, key, .. } => vec![obj, key],
    }
}

/// An expression's direct subexpressions, in evaluation order.
pub(super) fn children(e: &HExpr) -> Vec<&HExpr> {
    pub(super) fn elems(xs: &[HElem]) -> Vec<&HExpr> {
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
pub(super) fn math_native(n: Native) -> bool {
    matches!(n, Native::MathFloor | Native::MathCeil | Native::MathTrunc | Native::MathSqrt | Native::MathAbs) || MATH1.contains(&n) || MATH2.contains(&n)
}

/// Assignments (and initializations) of variables in `ss`, with their values.
/// A statement's own expressions and its nested statement lists.
pub(super) fn parts(s: &HStmt) -> (Vec<&HExpr>, Vec<&[HStmt]>) {
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

/// Does any expression in `ss` (at any depth) satisfy `f`?
pub(super) fn any_expr(ss: &[HStmt], f: &mut impl FnMut(&HExpr) -> bool) -> bool {
    pub(super) fn e(x: &HExpr, f: &mut impl FnMut(&HExpr) -> bool) -> bool {
        f(x) || children(x).into_iter().any(|c| e(c, f))
    }
    ss.iter().any(|s| {
        let (xs, bodies) = parts(s);
        xs.into_iter().any(|x| e(x, f)) || bodies.into_iter().any(|b| any_expr(b, f))
    })
}

/// Does any block in `ss` (at any depth, `ss` included) satisfy `f`?
pub(super) fn any_block(ss: &[HStmt], f: &mut impl FnMut(&[HStmt]) -> bool) -> bool {
    f(ss) || ss.iter().any(|s| parts(s).1.into_iter().any(|b| any_block(b, f)))
}

/// Variables declared anywhere inside `ss`, and its `for-of` statements' keys.
pub(super) fn block_decls(ss: &[HStmt], vars: &mut Vec<VarId>, iters: &mut Vec<usize>) {
    for s in ss {
        match s {
            HStmt::Init(v, _) | HStmt::MakeClosure(v, _) => vars.push(*v),
            HStmt::Block(_, vs) => vars.extend(vs),
            HStmt::ForOf { elem, .. } => {
                vars.push(*elem);
                iters.push(s as *const HStmt as usize);
            }
            _ => {}
        }
        for b in parts(s).1 {
            block_decls(b, vars, iters);
        }
    }
}

/// Every `v = x` of a variable: assignments, initializers, and a `for-of`
/// variable taking `iter`'s elements (`(elem, iter)`).
pub(super) fn collect_ref_assigns<'a>(ss: &'a [HStmt], out: &mut Vec<(VarId, &'a HExpr)>) {
    pub(super) fn e<'a>(x: &'a HExpr, out: &mut Vec<(VarId, &'a HExpr)>) {
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
pub(super) fn safe_borrow(x: &HExpr, set: &FxHashSet<VarId>) -> bool {
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
pub(super) fn is_var_read(hir: &HProgram, e: &HExpr) -> bool {
    match &e.kind {
        HK::Read(p) => matches!(**p, Place::Var(v) if hir.vars[v as usize].owner.is_some() && !hir.vars[v as usize].captured),
        HK::Cast(x) => is_var_read(hir, x),
        HK::Null | HK::Undefined | HK::Str(_) => true,
        _ => false,
    }
}

/// Variables assigned anywhere in `ss` (to find parameters that must own their value).
pub(super) fn assigned_vars(ss: &[HStmt], out: &mut FxHashSet<VarId>) {
    pub(super) fn e(x: &HExpr, out: &mut FxHashSet<VarId>) {
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
