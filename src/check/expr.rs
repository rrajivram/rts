//! Expression checking.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use super::builtins::{NsMember, global_fn_type, special_global_call};
use super::*;
use crate::ast::{ArrayElem, AssignOp, BinOp, Expr, ExprKind, LogicalOp, ObjProp, PropKey, UnaryOp};
use crate::runtime::Native;

type Subst = FxHashMap<TParamId, Type>;

fn is_fn_expr(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Func(_) => true,
        ExprKind::Paren(x) => is_fn_expr(x),
        _ => false,
    }
}

impl Checker {
    pub fn check_expr(&mut self, e: &Expr, want: Option<&Type>) -> CResult<HExpr> {
        let span = e.span;
        Ok(match &e.kind {
            ExprKind::Await(x) => self.check_await(x, want, e.span)?,
            ExprKind::Regex(src, flags) => {
                if let Err(m) = crate::runtime::regexp::RegExp::new(src, flags) {
                    return err(m, e.span);
                }
                let args = vec![HElem::Expr(HExpr::str(src)), HElem::Expr(HExpr::str(flags))];
                self.native_call(Native::RegexNew, None, args, Type::RegExp)
            }
            ExprKind::Yield { arg, delegate } => self.check_yield(arg.as_deref(), *delegate, e.span)?,
            ExprKind::Num(v) => HExpr::num(*v),
            ExprKind::BigInt(v) => HExpr::new(HK::BigInt(*v), Type::BigInt),
            ExprKind::Str(s) => HExpr::new(HK::Str(s.clone()), Type::StrLit(s.clone())),
            ExprKind::Bool(b) => HExpr::new(HK::Bool(*b), Type::BoolLit(*b)),
            ExprKind::Null => HExpr::new(HK::Null, Type::Null),
            ExprKind::Template { quasis, exprs } => self.check_template(quasis, exprs)?,
            ExprKind::Paren(x) => self.check_expr(x, want)?,
            ExprKind::Ident(n) => self.check_ident(n, span)?,
            ExprKind::This => self.check_this(span)?,
            ExprKind::Super => return err("'super' can only be used in calls and member access", span),
            ExprKind::Array(elems) => self.check_array(elems, want, span)?,
            ExprKind::Object(props) => self.check_object(props, want, span)?,
            ExprKind::Func(f) => self.check_func_expr(f, want)?,
            ExprKind::Class(c) => self.check_class_expr(c, span)?,
            ExprKind::Unary(op, x) => self.check_unary(*op, x, span)?,
            ExprKind::Update { inc, prefix, target } => {
                let place = self.check_place(target)?;
                let t = self.place_type(&place);
                if t.is_bigint() {
                    self.invalidate(target);
                    return Ok(HExpr::new(
                        HK::Update {
                            place: Box::new(place),
                            inc: *inc,
                            prefix: *prefix,
                        },
                        Type::BigInt,
                    ));
                }
                if !t.is_numeric() && !t.is_dynamic() {
                    return err(
                        format!("an arithmetic operand must be of type 'number', not '{}'", self.show(&t)),
                        target.span,
                    );
                }
                self.invalidate(target);
                HExpr::new(
                    HK::Update {
                        place: Box::new(place),
                        inc: *inc,
                        prefix: *prefix,
                    },
                    Type::Number,
                )
            }
            ExprKind::Binary(op, a, b) => self.check_binary(*op, a, b, span)?,
            ExprKind::Logical(op, a, b) => self.check_logical(*op, a, b, want)?,
            ExprKind::Assign(op, t, v) => self.check_assign(*op, t, v, span)?,
            ExprKind::Cond(c, a, b) => {
                let hc = self.check_expr(c, None)?;
                let (tn, fnn) = (self.refine(c, true)?, self.refine(c, false)?);
                let saved = self.ctx().narrow.clone();
                self.apply(&tn);
                let ha = self.check_expr(a, want);
                self.ctx_mut().narrow = saved.clone();
                let ha = ha?;
                self.apply(&fnn);
                let hb = self.check_expr(b, want);
                self.ctx_mut().narrow = saved;
                let hb = hb?;
                let ty = Type::union([ha.ty.clone(), hb.ty.clone()]);
                HExpr::new(HK::Cond(Box::new(hc), Box::new(ha), Box::new(hb)), ty)
            }
            ExprKind::Call {
                callee,
                targs,
                args,
                optional,
            } => self.check_call(callee, targs, args, *optional, span)?,
            ExprKind::New { callee, targs, args } => self.check_new(callee, targs, args, want, span)?,
            ExprKind::Member { obj, prop, optional } => self.check_member(obj, prop, *optional, span)?,
            ExprKind::Index { obj, index, optional } => self.check_index(obj, index, *optional, span)?,
            ExprKind::OptChain(x) => {
                let h = self.check_expr(x, want)?;
                let ty = Type::union([h.ty.clone(), Type::Undefined]);
                HExpr::new(HK::OptChain(Box::new(h)), ty)
            }
            ExprKind::As(x, te) => {
                let t = self.resolve_type(te)?;
                let h = self.check_expr(x, Some(&t))?;
                h.cast(t)
            }
            ExprKind::Satisfies(x, te) => {
                let t = self.resolve_type(te)?;
                let h = self.check_expr(x, Some(&t))?;
                self.expect_assignable(&h.ty, &t, x.span)?;
                h
            }
            ExprKind::NonNull(x) => {
                let h = self.check_expr(x, want)?;
                let t = h.ty.non_null();
                h.cast(t)
            }
            ExprKind::Seq(es) => {
                let mut hs = Vec::new();
                for (i, x) in es.iter().enumerate() {
                    hs.push(self.check_expr(x, if i + 1 == es.len() { want } else { None })?);
                }
                let ty = hs.last().unwrap().ty.clone();
                HExpr::new(HK::Seq(hs), ty)
            }
        })
    }

    fn check_await(&mut self, x: &Expr, want: Option<&Type>, span: Span) -> CResult<HExpr> {
        self.allow_await(span)?;
        let want_p = want.map(|w| Type::union([w.clone(), Type::promise(w.clone())]));
        let h = self.check_expr(x, want_p.as_ref())?;
        let t = if self.erase { Type::Any } else { h.ty.awaited() };
        Ok(HExpr::new(HK::Await(Box::new(h)), t))
    }

    /// `await` is allowed in async functions and at the top level of a module
    /// (which then runs as an async function).
    pub fn allow_await(&mut self, span: Span) -> CResult<()> {
        if self.ctx().func == MAIN {
            self.ctx_mut().is_async = true;
            return Ok(());
        }
        if !self.ctx().is_async {
            return err("'await' is only allowed in async functions and at the top level of a module", span);
        }
        Ok(())
    }

    fn check_yield(&mut self, arg: Option<&Expr>, delegate: bool, span: Span) -> CResult<HExpr> {
        if !self.ctx().is_generator {
            return err("'yield' is only allowed in generator functions", span);
        }
        let is_async = self.ctx().is_async;
        if delegate {
            let Some(x) = arg else { return err("'yield*' needs an operand", span) };
            if is_async {
                return err("'yield*' in async generators is not supported yet", span);
            }
            let h = self.check_expr(x, None)?;
            let Some(elem) = self.iter_elem(&h.ty) else {
                return err(format!("type '{}' is not iterable", self.show(&h.ty)), x.span);
            };
            self.ctx_mut().yields.push(elem);
            let t = match &h.ty {
                Type::Iter(it) => it.return_ty.clone(),
                _ => Type::Any,
            };
            let t = if self.erase || matches!(t, Type::Void) { Type::Any } else { t };
            return Ok(HExpr::new(HK::YieldStar(Box::new(h)), t));
        }
        let want = self.ctx().yield_ty.clone();
        let h = match arg {
            Some(x) => {
                let want_p = match (&want, is_async) {
                    (Some(w), true) => Some(Type::union([w.clone(), Type::promise(w.clone())])),
                    (w, _) => w.clone(),
                };
                self.check_expr(x, want_p.as_ref())?
            }
            None => HExpr::undefined(),
        };
        let yielded = if is_async { h.ty.awaited() } else { h.ty.clone() };
        if let Some(w) = &want {
            self.expect_assignable(&yielded, w, arg.map_or(span, |a| a.span))?;
        }
        self.ctx_mut().yields.push(yielded);
        let t = if self.erase { Type::Any } else { self.ctx().next_ty.clone() };
        Ok(HExpr::new(HK::Yield(Box::new(h)), t))
    }

    /// `class { … }` as an expression: declared in a scope of its own.
    fn check_class_expr(&mut self, c: &Rc<crate::ast::ClassDecl>, span: Span) -> CResult<HExpr> {
        let name: Name = c.name.clone().unwrap_or_else(|| "".into());
        let scope = Scope::new(Some(self.ctx().scope.clone()), self.ctx().scope.owner);
        let cid = self.declare_class(&name, c.clone(), &scope)?;
        let saved = self.ctx().scope.clone();
        self.ctx_mut().scope = scope.clone();
        let r = (|| -> CResult<Vec<HStmt>> {
            let mut decl = (**c).clone();
            decl.name = Some(name.clone());
            let stmt = crate::ast::Stmt {
                kind: crate::ast::StmtKind::Class(Rc::new(decl)),
                span,
            };
            let init = self.check_stmt(&stmt, None)?;
            self.check_class_members(cid)?;
            Ok(init)
        })();
        self.ctx_mut().scope = saved;
        let init = r?;
        let class_ref = HExpr::new(HK::ClassRef(cid), Type::ClassCtor(cid));
        if init.is_empty() {
            return Ok(class_ref);
        }
        // Static initializers run when the expression is evaluated.
        Ok(HExpr::new(HK::StmtExpr(init, Box::new(class_ref)), Type::ClassCtor(cid)))
    }

    /// An enum used as a value: its run-time object (numeric members also
    /// map back from value to name).
    fn enum_object(&mut self, eid: u32, span: Span) -> CResult<HExpr> {
        let members = self.enums[eid as usize].members.clone();
        let mut props = Vec::new();
        let lit = |kind: ExprKind| Expr { kind, span };
        for (m, v) in &members {
            if let EnumVal::Num(x) = v {
                props.push(ObjProp::Init {
                    key: PropKey::Name(crate::runtime::num_to_string(*x).into()),
                    value: lit(ExprKind::Str(m.clone())),
                });
            }
        }
        for (m, v) in &members {
            props.push(ObjProp::Init {
                key: PropKey::Name(m.clone()),
                value: lit(match v {
                    EnumVal::Num(x) => ExprKind::Num(*x),
                    EnumVal::Str(s) => ExprKind::Str(s.clone()),
                }),
            });
        }
        let h = self.check_expr(&lit(ExprKind::Object(props)), None)?;
        Ok(h)
    }

    fn check_template(&mut self, quasis: &[Name], exprs: &[Expr]) -> CResult<HExpr> {
        if exprs.is_empty() {
            return Ok(HExpr::new(HK::Str(quasis[0].clone()), Type::StrLit(quasis[0].clone())));
        }
        let mut acc: Option<HExpr> = None;
        let push = |acc: Option<HExpr>, x: HExpr| -> Option<HExpr> {
            Some(match acc {
                None => x,
                Some(a) => HExpr::new(HK::Binary(HBin::Concat, Box::new(a), Box::new(x)), Type::String),
            })
        };
        for (i, q) in quasis.iter().enumerate() {
            if !q.is_empty() {
                acc = push(acc, HExpr::str(q));
            }
            if let Some(x) = exprs.get(i) {
                let h = self.check_expr(x, None)?;
                acc = push(acc, self.to_string_expr(h));
            }
        }
        let mut r = acc.unwrap();
        r.ty = Type::String;
        Ok(r)
    }

    pub fn to_string_expr(&self, h: HExpr) -> HExpr {
        if h.ty.is_stringy() {
            h.cast(Type::String)
        } else {
            HExpr::new(HK::Unary(HUnary::ToStr, Box::new(h)), Type::String)
        }
    }

    fn check_ident(&mut self, n: &Name, span: Span) -> CResult<HExpr> {
        match &**n {
            "undefined" => return Ok(HExpr::undefined()),
            "NaN" => return Ok(HExpr::num(f64::NAN)),
            "Infinity" => return Ok(HExpr::num(f64::INFINITY)),
            _ => {}
        }
        match self.lookup_val(n) {
            Some(VB::Var(v)) => {
                if self.var_pending[v as usize] {
                    // Inside a nested function the read happens later, when the
                    // variable is initialized (its type isn't known yet).
                    let owner = self.vars[v as usize].owner.unwrap_or(MAIN);
                    if owner == self.ctx().func || (self.vars[v as usize].owner.is_none() && self.ctx().func == MAIN) {
                        return err(format!("variable '{n}' is used before its declaration"), span);
                    }
                    self.note_var_use(v);
                    return Ok(HExpr::read(Place::Var(v), Type::Any));
                }
                if let Some(c) = &self.var_const_val[v as usize] {
                    return Ok(c.clone());
                }
                self.note_var_use(v);
                let key = format!("{v}");
                let ty = match self.ctx().narrow.get(&key) {
                    Some(t) => t.clone(),
                    None => self.vars[v as usize].ty.clone(),
                };
                Ok(HExpr::read(Place::Var(v), ty))
            }
            Some(VB::Func(v, f)) => {
                let ft = self.func_type(f)?;
                let ty = Type::Func(ft);
                self.vars[v as usize].ty = ty.clone();
                self.var_pending[v as usize] = false;
                self.note_var_use(v);
                Ok(HExpr::read(Place::Var(v), ty))
            }
            Some(VB::Class(c)) => Ok(HExpr::new(HK::ClassRef(c), Type::ClassCtor(c))),
            Some(VB::Enum(eid)) => self.enum_object(eid, span),
            Some(VB::Builtin(ns)) => match ns {
                BuiltinNs::String => Ok(HExpr::new(HK::NativeFn(Native::StringFn), Type::Func(global_fn_type(Native::StringFn)))),
                BuiltinNs::Number => Ok(HExpr::new(HK::NativeFn(Native::NumberFn), Type::Func(global_fn_type(Native::NumberFn)))),
                BuiltinNs::Boolean => Ok(HExpr::new(HK::NativeFn(Native::BooleanFn), Type::Func(global_fn_type(Native::BooleanFn)))),
                _ => err(format!("'{n}' cannot be used as a value"), span),
            },
            Some(VB::NativeFn(f)) => Ok(HExpr::new(HK::NativeFn(f), Type::Func(global_fn_type(f)))),
            Some(VB::Namespace(m)) => self.namespace_object(m, span),
            None if &**n == "process" => Ok(self.native_call(Native::ProcessObject, None, vec![], Type::Any)),
            None if &**n == "globalThis" => Ok(self.native_call(Native::GlobalThis, None, vec![], Type::Any)),
            None => err(format!("cannot find name '{n}'"), span),
        }
    }

    /// The `this` variable visible here (arrows see their enclosing function's).
    fn this_binding(&self) -> Option<(VarId, Type)> {
        for ctx in self.stack.iter().rev() {
            if ctx.kind == FnKind::Arrow {
                continue;
            }
            return ctx.this_var.map(|v| (v, ctx.this_ty.clone().unwrap_or(Type::Any)));
        }
        None
    }

    fn check_this(&mut self, _span: Span) -> CResult<HExpr> {
        match self.this_binding() {
            Some((v, t)) => {
                self.note_var_use(v);
                let key = format!("{v}");
                let ty = self.ctx().narrow.get(&key).cloned().unwrap_or(t);
                let ty = if self.erase { Type::Any } else { ty };
                Ok(HExpr::read(Place::Var(v), ty))
            }
            None => Ok(HExpr::undefined()),
        }
    }

    fn check_array(&mut self, elems: &[ArrayElem], want: Option<&Type>, span: Span) -> CResult<HExpr> {
        // Contextual element type(s).
        let want = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Array(_) | Type::Tuple(_))));
        let (elem_ctx, tuple_ctx) = match &want {
            Some(Type::Array(t)) => (Some((**t).clone()), None),
            Some(Type::Tuple(ts)) => (None, Some(ts.clone())),
            _ => (None, None),
        };
        let mut hs = Vec::new();
        let mut tys = Vec::new();
        for (i, el) in elems.iter().enumerate() {
            match el {
                ArrayElem::Expr(x) => {
                    let ctx = elem_ctx.clone().or_else(|| tuple_ctx.as_ref().and_then(|t| t.get(i).cloned()));
                    let h = self.check_expr(x, ctx.as_ref())?;
                    if let Some(c) = &ctx {
                        self.expect_assignable(&h.ty, c, x.span)?;
                    }
                    tys.push(h.ty.clone());
                    hs.push(HElem::Expr(h));
                }
                ArrayElem::Spread(x) => {
                    let h = self.check_expr(x, want.as_ref())?;
                    let Some(et) = self.iter_elem(&h.ty) else {
                        return err(format!("type '{}' is not iterable", self.show(&h.ty)), x.span);
                    };
                    tys.push(et);
                    hs.push(HElem::Spread(h));
                }
                ArrayElem::Hole => {
                    tys.push(Type::Undefined);
                    hs.push(HElem::Expr(HExpr::undefined()));
                }
            }
        }
        let ty = match (&want, elem_ctx, tuple_ctx) {
            (_, Some(t), _) => Type::array(t),
            (_, _, Some(ts)) if ts.len() == elems.len() => Type::Tuple(ts),
            _ => {
                if tys.is_empty() {
                    Type::array(Type::Any)
                } else {
                    Type::array(Type::union(tys.iter().map(|t| t.widen())))
                }
            }
        };
        let _ = span;
        Ok(HExpr::new(HK::Array(hs), ty))
    }

    /// For a contextual union type, pick the member matching `f` (or the type itself).
    fn pick_member(&self, t: &Type, f: impl Fn(&Type) -> bool) -> Type {
        match t {
            Type::Union(ts) => ts.iter().find(|m| f(m)).cloned().unwrap_or_else(|| t.clone()),
            t => t.clone(),
        }
    }

    fn check_object(&mut self, props: &[ObjProp], want: Option<&Type>, span: Span) -> CResult<HExpr> {
        let want_obj = want.and_then(|w| {
            let w = self.pick_member(w, |t| matches!(t, Type::Object(_) | Type::Iface(..) | Type::Class(..)));
            self.obj_view(&w)
        });
        let keep_literals = want.is_some();
        let mut hs = Vec::new();
        let mut out_props: Vec<Prop> = Vec::new();
        let mut computed: Vec<Type> = Vec::new();
        for p in props {
            match p {
                ObjProp::Accessor { key, is_get, func } => {
                    let this_ty = want.cloned().unwrap_or(Type::Any);
                    let scope = self.ctx().scope.clone();
                    let fid = self.new_func(key.clone(), Some(func.clone()), FnKind::ObjMethod, None, None, scope, Some(this_ty));
                    self.ensure_checked(fid)?;
                    let ft = self.func_type(fid)?;
                    let ty = if *is_get { ft.ret.clone() } else { ft.params.first().map_or(Type::Any, |p| p.ty.clone()) };
                    let closure = HExpr::new(HK::Closure(fid), Type::Func(ft));
                    // Merge with the other half of a get/set pair.
                    let existing = hs.iter_mut().find_map(|h| match h {
                        HProp::Accessor { name, get, set } if name == key => Some((get, set)),
                        _ => None,
                    });
                    match existing {
                        Some((get, set)) => {
                            if *is_get {
                                *get = Some(closure);
                            } else {
                                *set = Some(closure);
                            }
                        }
                        None => {
                            let (get, set) = if *is_get { (Some(closure), None) } else { (None, Some(closure)) };
                            hs.push(HProp::Accessor { name: key.clone(), get, set });
                        }
                    }
                    if !out_props.iter().any(|q| &q.name == key) || *is_get {
                        out_props.retain(|q| &q.name != key);
                        out_props.push(Prop {
                            name: key.clone(),
                            ty,
                            optional: false,
                        });
                    }
                }
                ObjProp::Init { key, value } => {
                    let name = match key {
                        PropKey::Name(n) => n.clone(),
                        PropKey::Computed(k) => match &k.kind {
                            ExprKind::Str(s) => s.clone(),
                            ExprKind::Num(n) => crate::runtime::num_to_string(*n).into(),
                            _ => {
                                // A key known only at run time: the object gets an index signature.
                                let hk = self.check_expr(k, None)?;
                                let hv = self.check_expr(value, None)?;
                                computed.push(hv.ty.widen());
                                hs.push(HProp::Computed(hk, hv));
                                continue;
                            }
                        },
                    };
                    let ctx = want_obj
                        .as_ref()
                        .and_then(|o| o.prop(&name).map(|p| p.ty.clone()).or_else(|| o.index.clone()));
                    let h = if let ExprKind::Func(f) = &value.kind
                        && !f.is_arrow
                    {
                        // Object literal method: `this` is the object.
                        let ctxf = ctx.as_ref().and_then(|t| match t {
                            Type::Func(f) => Some(f.clone()),
                            _ => None,
                        });
                        let this_ty = want.cloned().unwrap_or(Type::Any);
                        let scope = self.ctx().scope.clone();
                        let fid = self.new_func(name.clone(), Some(f.clone()), FnKind::ObjMethod, None, ctxf, scope, Some(this_ty));
                        self.inherit_narrowing(fid);
                        self.ensure_checked(fid)?;
                        let ft = self.func_type(fid)?;
                        HExpr::new(HK::Closure(fid), Type::Func(ft))
                    } else {
                        self.check_expr(value, ctx.as_ref())?
                    };
                    let ty = if keep_literals { h.ty.clone() } else { h.ty.widen() };
                    out_props.retain(|q| q.name != name);
                    out_props.push(Prop {
                        name: name.clone(),
                        ty,
                        optional: false,
                    });
                    hs.push(HProp::Init(name, h));
                }
                ObjProp::Spread(x) => {
                    let h = self.check_expr(x, None)?;
                    if let Some(o) = self.obj_view(&h.ty.non_null()) {
                        for p in &o.props {
                            out_props.retain(|q| q.name != p.name);
                            out_props.push(p.clone());
                        }
                    } else if !h.ty.is_dynamic() {
                        return err(format!("cannot spread type '{}'", self.show(&h.ty)), x.span);
                    }
                    hs.push(HProp::Spread(h));
                }
            }
        }
        let _ = span;
        let index = if computed.is_empty() {
            None
        } else {
            computed.extend(out_props.iter().map(|p| p.ty.clone()));
            Some(Type::union(computed))
        };
        Ok(HExpr::new(
            HK::Object(hs),
            Type::Object(Rc::new(ObjType {
                props: out_props,
                index,
                call: None,
            })),
        ))
    }

    /// Arrow functions keep narrowings of `const` variables from their context.
    fn inherit_narrowing(&mut self, _fid: FuncId) {
        let n: Narrow = self
            .ctx()
            .narrow
            .iter()
            .filter(|(k, _)| {
                let root: VarId = k.split('.').next().unwrap().parse().unwrap_or(u32::MAX);
                (root as usize) < self.var_const.len() && self.var_const[root as usize]
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.pending_narrow = Some(n);
    }

    pub fn check_func_expr(&mut self, f: &Rc<ast::Function>, want: Option<&Type>) -> CResult<HExpr> {
        let ctxf = want.and_then(|w| {
            let w = self.pick_member(w, |t| matches!(t, Type::Func(_)));
            match w {
                Type::Func(f) => Some(f),
                ref t => self.obj_view(t).and_then(|o| o.call.clone()),
            }
        });
        let kind = if f.is_arrow { FnKind::Arrow } else { FnKind::Normal };
        let mut scope = self.ctx().scope.clone();
        let name = f.name.clone().unwrap_or_else(|| "".into());
        // A named function expression can refer to itself: bind its name in a
        // scope of its own, backed by a hidden variable in the current block.
        let self_var = match &f.name {
            Some(n) if !f.is_arrow => {
                let v = self.new_var(format!("$fn:{n}").into(), None, true, &scope);
                let inner = Scope::new(Some(scope.clone()), scope.owner);
                scope = inner;
                Some((n.clone(), v))
            }
            _ => None,
        };
        let fid = self.new_func(name.clone(), Some(f.clone()), kind, None, ctxf.clone(), scope.clone(), None);
        if let Some((n, v)) = &self_var {
            scope.vals.borrow_mut().insert(n.clone(), VB::Func(*v, fid));
        }
        self.inherit_narrowing(fid);
        // Check the body now: the enclosing context is on the stack (for `this`).
        if let Some(c) = &ctxf
            && f.ret.is_none()
            && !matches!(c.ret, Type::Void | Type::Any)
            && !contains_param(&c.ret)
        {
            // Contextual return type: check returns against it.
            self.funcs[fid as usize].ret = None;
        }
        self.ensure_checked(fid)?;
        let ft = self.func_type(fid)?;
        let closure = HExpr::new(HK::Closure(fid), Type::Func(ft.clone()));
        if let Some((_, v)) = self_var {
            self.vars[v as usize].ty = Type::Func(ft.clone());
            self.var_pending[v as usize] = false;
            return Ok(HExpr::new(HK::Assign(Box::new(Place::Var(v)), Box::new(closure)), Type::Func(ft)));
        }
        Ok(closure)
    }

    fn check_unary(&mut self, op: UnaryOp, x: &Expr, span: Span) -> CResult<HExpr> {
        if op == UnaryOp::Delete {
            return match &x.kind {
                ExprKind::Member { obj, prop, .. } => {
                    let o = self.check_expr(obj, None)?;
                    Ok(HExpr::new(HK::Delete(Box::new(o), Box::new(HExpr::str(prop))), Type::Boolean))
                }
                ExprKind::Index { obj, index, .. } => {
                    let o = self.check_expr(obj, None)?;
                    let k = self.check_expr(index, None)?;
                    Ok(HExpr::new(HK::Delete(Box::new(o), Box::new(k)), Type::Boolean))
                }
                _ => err("delete requires a property access", span),
            };
        }
        let h = self.check_expr(x, None)?;
        let t = h.ty.clone();
        if t.is_bigint() && matches!(op, UnaryOp::Neg | UnaryOp::BitNot) {
            if let (UnaryOp::Neg, HK::BigInt(v)) = (op, &h.kind)
                && let Some(n) = v.checked_neg()
            {
                return Ok(HExpr::new(HK::BigInt(n), Type::BigInt));
            }
            let u = if op == UnaryOp::Neg { HUnary::NegBig } else { HUnary::BitNotBig };
            return Ok(HExpr::new(HK::Unary(u, Box::new(h)), Type::BigInt));
        }
        if t.is_dynamic() && matches!(op, UnaryOp::Neg | UnaryOp::BitNot) {
            let u = if op == UnaryOp::Neg { HUnary::NegDyn } else { HUnary::BitNotDyn };
            return Ok(HExpr::new(HK::Unary(u, Box::new(h)), Type::Any));
        }
        if t.is_bigint() && op == UnaryOp::Plus {
            return err("operator '+' cannot be applied to type 'bigint' (use Number(x))", span);
        }
        Ok(match op {
            UnaryOp::Neg | UnaryOp::Plus | UnaryOp::BitNot => {
                let operand = if t.is_numeric() {
                    h
                } else if t.is_dynamic() || op == UnaryOp::Plus {
                    HExpr::new(HK::Unary(HUnary::ToNum, Box::new(h)), Type::Number)
                } else {
                    return err(format!("operator cannot be applied to type '{}'", self.show(&t)), span);
                };
                match op {
                    UnaryOp::Neg => {
                        if let HK::Num(v) = operand.kind {
                            return Ok(HExpr::num(-v));
                        }
                        HExpr::new(HK::Unary(HUnary::NegNum, Box::new(operand)), Type::Number)
                    }
                    UnaryOp::Plus => operand.cast(Type::Number),
                    _ => HExpr::new(HK::Unary(HUnary::BitNotNum, Box::new(operand)), Type::Number),
                }
            }
            UnaryOp::Not => HExpr::new(HK::Unary(HUnary::NotBool, Box::new(h)), Type::Boolean),
            UnaryOp::TypeOf => HExpr::new(HK::Unary(HUnary::TypeOf, Box::new(h)), Type::String),
            UnaryOp::Void => HExpr::new(HK::Unary(HUnary::Void, Box::new(h)), Type::Undefined),
            UnaryOp::Delete => unreachable!(),
        })
    }

    pub fn arith_operand(&mut self, h: HExpr, span: Span) -> CResult<HExpr> {
        if h.ty.is_numeric() {
            return Ok(h);
        }
        if h.ty.is_dynamic() || matches!(h.ty, Type::Param(_)) {
            return Ok(HExpr::new(HK::Unary(HUnary::ToNum, Box::new(h)), Type::Number));
        }
        err(
            format!(
                "the operand of an arithmetic operation must be of type 'number', not '{}'",
                self.show(&h.ty)
            ),
            span,
        )
    }

    pub fn bin_types(&mut self, op: BinOp, ha: HExpr, hb: HExpr, sa: Span, sb: Span) -> CResult<HExpr> {
        use BinOp::*;
        let (ta, tb) = (ha.ty.clone(), hb.ty.clone());
        let numop = |op: BinOp| match op {
            Add => NumOp::Add,
            Sub => NumOp::Sub,
            Mul => NumOp::Mul,
            Div => NumOp::Div,
            Mod => NumOp::Mod,
            Exp => NumOp::Exp,
            BitAnd => NumOp::BitAnd,
            BitOr => NumOp::BitOr,
            BitXor => NumOp::BitXor,
            Shl => NumOp::Shl,
            Shr => NumOp::Shr,
            _ => NumOp::UShr,
        };
        let cmpop = |op: BinOp| match op {
            Lt => CmpOp::Lt,
            Le => CmpOp::Le,
            Gt => CmpOp::Gt,
            _ => CmpOp::Ge,
        };
        let mk = |op: HBin, a: HExpr, b: HExpr, ty: Type| HExpr::new(HK::Binary(op, Box::new(a), Box::new(b)), ty);
        // BigInts: unboxed when both sides are `bigint`; mixing with numbers is
        // a type error (as in TypeScript), except in comparisons.
        let (big_a, big_b) = (ta.is_bigint(), tb.is_bigint());
        if big_a || big_b {
            let other = if big_a { &tb } else { &ta };
            let arith = matches!(op, Add | Sub | Mul | Div | Mod | Exp | BitAnd | BitOr | BitXor | Shl | Shr | UShr);
            if big_a && big_b {
                match op {
                    UShr => return err("operator '>>>' cannot be applied to bigints (use >>)", sa.to(sb)),
                    _ if arith => return Ok(mk(HBin::Big(numop(op)), ha, hb, Type::BigInt)),
                    Lt | Le | Gt | Ge => return Ok(mk(HBin::BigCmp(cmpop(op)), ha, hb, Type::Boolean)),
                    StrictEq | Eq => return Ok(mk(HBin::BigCmp(CmpOp::Eq), ha, hb, Type::Boolean)),
                    StrictNe | Ne => return Ok(mk(HBin::BigCmp(CmpOp::Ne), ha, hb, Type::Boolean)),
                    _ => {}
                }
            } else if arith && !other.is_dynamic() && !(op == Add && other.is_stringy()) {
                return err(
                    format!("operator cannot be applied to types '{}' and '{}' (convert with BigInt(x) or Number(x))", self.show(&ta), self.show(&tb)),
                    sa.to(sb),
                );
            } else if matches!(op, Lt | Le | Gt | Ge) {
                return Ok(mk(HBin::DynCmp(cmpop(op)), ha, hb, Type::Boolean));
            }
        }
        Ok(match op {
            Add => {
                if ta.is_numeric() && tb.is_numeric() {
                    mk(HBin::Num(NumOp::Add), ha, hb, Type::Number)
                } else if ta.is_stringy() || tb.is_stringy() {
                    let a = self.to_string_expr(ha);
                    let b = self.to_string_expr(hb);
                    mk(HBin::Concat, a, b, Type::String)
                } else if ta.is_dynamic() || tb.is_dynamic() {
                    mk(HBin::DynAdd, ha, hb, Type::Any)
                } else {
                    let ok = |t: &Type| t.members().iter().all(|m| m.is_numeric() || m.is_stringy());
                    if ok(&ta) && ok(&tb) {
                        mk(HBin::DynAdd, ha, hb, Type::union([Type::String, Type::Number]))
                    } else {
                        return err(
                            format!("operator '+' cannot be applied to types '{}' and '{}'", self.show(&ta), self.show(&tb)),
                            sa.to(sb),
                        );
                    }
                }
            }
            Sub | Mul | Div | Mod | Exp | BitAnd | BitOr | BitXor | Shl | Shr | UShr => {
                if ta.is_dynamic() || tb.is_dynamic() {
                    if !ta.is_numeric() && !tb.is_numeric() {
                        return Ok(mk(HBin::DynNumeric(numop(op)), ha, hb, Type::Any));
                    }
                    return Ok(mk(HBin::DynNum(numop(op)), ha, hb, Type::Number));
                }
                let a = self.arith_operand(ha, sa)?;
                let b = self.arith_operand(hb, sb)?;
                mk(HBin::Num(numop(op)), a, b, Type::Number)
            }
            Lt | Le | Gt | Ge => {
                if ta.is_numeric() && tb.is_numeric() {
                    mk(HBin::NumCmp(cmpop(op)), ha, hb, Type::Boolean)
                } else if ta.is_stringy() && tb.is_stringy() {
                    mk(HBin::StrCmp(cmpop(op)), ha, hb, Type::Boolean)
                } else {
                    mk(HBin::DynCmp(cmpop(op)), ha, hb, Type::Boolean)
                }
            }
            StrictEq | StrictNe => {
                let eq = op == StrictEq;
                if ta.is_numeric() && tb.is_numeric() {
                    mk(HBin::NumCmp(if eq { CmpOp::Eq } else { CmpOp::Ne }), ha, hb, Type::Boolean)
                } else if ta.is_booleanish() && tb.is_booleanish() {
                    mk(HBin::BoolEq(eq), ha, hb, Type::Boolean)
                } else {
                    mk(HBin::StrictEq(eq), ha, hb, Type::Boolean)
                }
            }
            Eq | Ne => {
                let eq = op == Eq;
                if ta.is_numeric() && tb.is_numeric() {
                    mk(HBin::NumCmp(if eq { CmpOp::Eq } else { CmpOp::Ne }), ha, hb, Type::Boolean)
                } else {
                    mk(HBin::LooseEq(eq), ha, hb, Type::Boolean)
                }
            }
            In => mk(HBin::In, ha, hb, Type::Boolean),
            InstanceOf => unreachable!(),
        })
    }

    fn check_binary(&mut self, op: BinOp, a: &Expr, b: &Expr, span: Span) -> CResult<HExpr> {
        if op == BinOp::InstanceOf {
            let ha = self.check_expr(a, None)?;
            return match &b.kind {
                ExprKind::Ident(n) => match self.lookup_val(n) {
                    Some(VB::Class(c)) => Ok(HExpr::new(HK::InstanceOf(Box::new(ha), c), Type::Boolean)),
                    Some(VB::Builtin(BuiltinNs::Array)) => Ok(self.native_call(Native::ArrIsArray, None, vec![HElem::Expr(ha)], Type::Boolean)),
                    Some(VB::Builtin(BuiltinNs::Promise | BuiltinNs::MapCtor | BuiltinNs::SetCtor | BuiltinNs::Object | BuiltinNs::RegExp)) | None
                        if matches!(&**n, "Promise" | "Map" | "Set" | "Object" | "Function" | "Date" | "RegExp" | "Symbol") =>
                    {
                        let args = vec![HElem::Expr(ha), HElem::Expr(HExpr::str(n))];
                        Ok(self.native_call(Native::IsInstanceOfBuiltin, None, args, Type::Boolean))
                    }
                    _ => err(format!("'instanceof {n}' is not supported"), b.span),
                },
                _ => err("the right side of 'instanceof' must be a class name", b.span),
            };
        }
        let ha = self.check_expr(a, None)?;
        let hb = self.check_expr(b, None)?;
        let _ = span;
        self.bin_types(op, ha, hb, a.span, b.span)
    }

    pub fn native_call(&mut self, n: Native, this: Option<HExpr>, args: Vec<HElem>, ret: Type) -> HExpr {
        HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Native(n),
                this,
                args,
                param_kinds: None,
                rest_at: None,
                ret_kind: Kind::Val,
                opt: false,
            })),
            ret,
        )
    }

    fn check_logical(&mut self, op: LogicalOp, a: &Expr, b: &Expr, want: Option<&Type>) -> CResult<HExpr> {
        let ha = self.check_expr(a, want)?;
        let saved = self.ctx().narrow.clone();
        match op {
            LogicalOp::And => {
                let r = self.refine(a, true)?;
                self.apply(&r);
            }
            LogicalOp::Or => {
                let r = self.refine(a, false)?;
                self.apply(&r);
            }
            LogicalOp::Nullish => {}
        }
        let hb = self.check_expr(b, want.or(Some(&ha.ty.clone())).filter(|_| op != LogicalOp::And));
        self.ctx_mut().narrow = saved;
        let hb = hb?;
        let ty = match op {
            LogicalOp::And => Type::union([falsy_part(&ha.ty), hb.ty.clone()]),
            LogicalOp::Or => Type::union([truthy_part(&ha.ty), hb.ty.clone()]),
            LogicalOp::Nullish => Type::union([ha.ty.non_null(), hb.ty.clone()]),
        };
        // `boolean && boolean` stays boolean (scalar).
        let ty = if ha.ty.is_booleanish() && hb.ty.is_booleanish() { Type::Boolean } else { ty };
        Ok(HExpr::new(HK::Logical(op, Box::new(ha), Box::new(hb)), ty))
    }

    // ───────────── places ─────────────

    /// Type of a place (declared type, for assignment checks).
    pub fn place_type(&self, p: &Place) -> Type {
        match p {
            Place::Var(v) => self.vars[*v as usize].ty.clone(),
            _ => Type::Any,
        }
    }

    pub fn check_place(&mut self, e: &Expr) -> CResult<Place> {
        let h = self.check_expr(e, None)?;
        let ty = h.ty.clone();
        match h.kind {
            HK::Read(p) => {
                if let Place::Var(v) = &*p
                    && self.var_const[*v as usize]
                {
                    return err(format!("cannot assign to '{}' because it is a constant", self.vars[*v as usize].name), e.span);
                }
                if let Place::StrLen { .. } = &*p {
                    return err("cannot assign to 'length' of a string", e.span);
                }
                if let Place::Accessor { set: None, .. } = &*p {
                    return err("cannot assign to a property that only has a getter", e.span);
                }
                let _ = ty;
                Ok(*p)
            }
            HK::Cast(inner) => match inner.kind {
                HK::Read(p) => Ok(*p),
                _ => err("invalid assignment target", e.span),
            },
            _ => err("invalid assignment target", e.span),
        }
    }

    /// Declared type of an assignment target expression.
    fn target_type(&mut self, e: &Expr, p: &Place) -> CResult<Type> {
        Ok(match p {
            Place::Var(v) => self.vars[*v as usize].ty.clone(),
            Place::ArrLen { .. } => Type::Number,
            _ => {
                // Re-derive the un-narrowed member type.
                let saved = self.ctx().narrow.clone();
                if let Some(k) = self.narrow_key(e) {
                    self.ctx_mut().narrow.remove(&k);
                }
                let h = self.check_expr(e, None);
                self.ctx_mut().narrow = saved;
                h?.ty
            }
        })
    }

    fn check_assign(&mut self, op: AssignOp, target: &Expr, value: &Expr, span: Span) -> CResult<HExpr> {
        // Array destructuring assignment: `[a, b] = [b, a]`.
        if let (AssignOp::Assign, ExprKind::Array(targets)) = (op, &target.kind) {
            return self.check_destructure_assign(targets, value, span);
        }
        let place = self.check_place(target)?;
        let tty = self.target_type(target, &place)?;
        match op {
            AssignOp::Assign => {
                let hv = self.check_expr(value, Some(&tty))?;
                self.expect_assignable(&hv.ty, &tty, value.span)?;
                self.invalidate(target);
                // Assignment narrowing.
                if let Place::Var(v) = &place
                    && matches!(tty, Type::Union(_))
                    && !hv.ty.is_dynamic()
                {
                    let key = format!("{v}");
                    let nt = hv.ty.widen();
                    if self.assignable(&nt, &tty) {
                        self.ctx_mut().narrow.insert(key, nt);
                    }
                }
                let ty = hv.ty.clone();
                Ok(HExpr::new(HK::Assign(Box::new(place), Box::new(hv.cast(tty))), ty))
            }
            AssignOp::Bin(b) => {
                let cur = HExpr::read(place.clone(), tty.clone());
                let hv = self.check_expr(value, None)?;
                let combined = self.bin_types(b, cur, hv.clone(), target.span, value.span)?;
                let HK::Binary(hop, _, rhs) = combined.kind else { unreachable!() };
                self.expect_assignable(&combined.ty, &tty, span)?;
                self.invalidate(target);
                Ok(HExpr::new(HK::Compound(Box::new(place), hop, rhs), combined.ty))
            }
            AssignOp::Logical(l) => {
                let hv = self.check_expr(value, Some(&tty))?;
                self.expect_assignable(&hv.ty, &tty, value.span)?;
                self.invalidate(target);
                Ok(HExpr::new(HK::LogicalAssign(Box::new(place), l, Box::new(hv.cast(tty.clone()))), tty))
            }
        }
    }

    fn check_destructure_assign(&mut self, targets: &[ArrayElem], value: &Expr, span: Span) -> CResult<HExpr> {
        let hv = self.check_expr(value, None)?;
        let scope = self.ctx().scope.clone();
        let tmp = self.new_var("$tmp".into(), Some(hv.ty.clone()), false, &scope);
        self.note_var_use(tmp);
        let tmp_read = HExpr::read(Place::Var(tmp), hv.ty.clone());
        let mut seq = vec![HExpr::new(HK::Assign(Box::new(Place::Var(tmp)), Box::new(hv.clone())), hv.ty.clone())];
        for (i, t) in targets.iter().enumerate() {
            let ArrayElem::Expr(te) = t else {
                if matches!(t, ArrayElem::Hole) {
                    continue;
                }
                return err("rest elements in destructuring assignment are not supported", span);
            };
            let place = self.check_place(te)?;
            let tty = self.target_type(te, &place)?;
            let elem = self.index_read(tmp_read.clone(), HExpr::num(i as f64), te.span)?;
            self.expect_assignable(&elem.ty, &tty, te.span)?;
            self.invalidate(te);
            seq.push(HExpr::new(HK::Assign(Box::new(place), Box::new(elem.cast(tty.clone()))), tty));
        }
        seq.push(tmp_read);
        Ok(HExpr::new(HK::Seq(seq), hv.ty))
    }

    // ───────────── members ─────────────

    fn check_member(&mut self, obj: &Expr, prop: &Name, opt: bool, span: Span) -> CResult<HExpr> {
        if let ExprKind::Super = obj.kind {
            return err("super property access is only supported in calls", span);
        }
        // Static access through a name.
        if let ExprKind::Ident(n) = &obj.kind {
            match self.lookup_val(n) {
                Some(VB::Builtin(ns)) => {
                    return match self.ns_member(ns, prop) {
                        Some(NsMember::Const(h)) => Ok(h),
                        Some(NsMember::Method(nat, ft)) => Ok(HExpr::new(HK::NativeFn(nat), Type::Func(ft))),
                        None => err(format!("property '{prop}' does not exist on '{n}'"), span),
                    };
                }
                Some(VB::Enum(eid)) => {
                    let e = &self.enums[eid as usize];
                    let Some((_, v)) = e.members.iter().find(|(m, _)| m == prop) else {
                        return err(format!("enum '{n}' has no member '{prop}'"), span);
                    };
                    return Ok(match v.clone() {
                        EnumVal::Num(x) => HExpr::new(HK::Num(x), self.enum_type(eid)),
                        EnumVal::Str(s) => HExpr::new(HK::Str(s.clone()), Type::StrLit(s)),
                    });
                }
                Some(VB::Class(cid)) => return self.static_member(cid, prop, span),
                None if matches!(&**n, "performance") => {
                    return err(format!("'{n}.{prop}' is only supported as a call"), span);
                }
                _ => {}
            }
        }
        let h = self.check_expr(obj, None)?;
        let r = self.member_read(h, prop, opt, span)?;
        // Narrowed property path?
        if let Some(k) = self.narrow_key_member(obj, prop)
            && let Some(t) = self.ctx().narrow.get(&k).cloned()
        {
            return Ok(r.cast(t));
        }
        Ok(r)
    }

    fn static_member(&mut self, cid: ClassId, prop: &Name, span: Span) -> CResult<HExpr> {
        self.ensure_class(cid)?;
        match self.classes[cid as usize].statics.get(prop).cloned() {
            Some(VB::Var(v)) => {
                self.note_var_use(v);
                Ok(HExpr::read(Place::Var(v), self.vars[v as usize].ty.clone()))
            }
            Some(VB::Func(v, f)) => {
                let ft = self.func_type(f)?;
                self.vars[v as usize].ty = Type::Func(ft.clone());
                self.var_pending[v as usize] = false;
                self.note_var_use(v);
                Ok(HExpr::read(Place::Var(v), Type::Func(ft)))
            }
            _ => err(
                format!("property '{prop}' does not exist on 'typeof {}'", self.classes[cid as usize].name),
                span,
            ),
        }
    }

    /// `h.prop` for an already-checked receiver.
    pub fn member_read(&mut self, h: HExpr, prop: &Name, opt: bool, span: Span) -> CResult<HExpr> {
        let t = self.norm(&h.ty.non_null());
        if let Type::Deferred(_) = t {
            return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, Type::Any));
        }
        // `T extends { length: number }`: read through the constraint.
        if let Type::Param(id) = &t
            && let Some(c) = self.tparams[*id as usize].constraint.clone()
        {
            return self.member_read(h.cast(c), prop, opt, span);
        }
        let wrap_opt = |ty: Type| if opt { Type::union([ty, Type::Undefined]) } else { ty };
        match &t {
            Type::Any => return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, Type::Any)),
            Type::Unknown => return err("object is of type 'unknown'", span),
            Type::Array(_) | Type::Tuple(_) if &**prop == "length" => {
                return Ok(HExpr::read(Place::ArrLen { obj: h, opt }, wrap_opt(Type::Number)));
            }
            Type::String | Type::StrLit(_) if &**prop == "length" => {
                return Ok(HExpr::read(Place::StrLen { obj: h, opt }, wrap_opt(Type::Number)));
            }
            // Properties added to a function object (`assert.ok = ok`).
            Type::Func(_) if !matches!(&**prop, "bind" | "call" | "apply") => {
                return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, Type::Any));
            }
            Type::RegExp => {
                let ty = match &**prop {
                    "lastIndex" => Type::Number,
                    "source" | "flags" => Type::String,
                    "global" | "sticky" | "ignoreCase" | "multiline" | "unicode" | "dotAll" => Type::Boolean,
                    _ => return err(format!("property '{prop}' does not exist on type 'RegExp'"), span),
                };
                return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, wrap_opt(ty)));
            }
            Type::Symbol if &**prop == "description" => {
                return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, Type::union([Type::String, Type::Undefined])));
            }
            Type::Map(..) | Type::Set(_) if &**prop == "size" => {
                let n = if matches!(t, Type::Map(..)) { Native::MapSize } else { Native::SetSize };
                let mut c = self.native_call(n, Some(h), vec![], Type::Number);
                if let HK::Call(call) = &mut c.kind {
                    call.opt = opt;
                }
                c.ty = wrap_opt(Type::Number);
                return Ok(c);
            }
            Type::ClassCtor(cid) => return self.static_member(*cid, prop, span),
            Type::Class(cid, args) => {
                let cid = *cid;
                let args = args.clone();
                self.ensure_class(cid)?;
                let map = self.class_subst(cid, &args);
                let c = &self.classes[cid as usize];
                if let Some(slot) = c.fields.iter().position(|f| f.name == *prop) {
                    let fty = c.fields[slot].ty.clone().unwrap_or(Type::Any);
                    let fty = self.subst(&fty, &map);
                    return Ok(HExpr::read(Place::Field { obj: h, slot: slot as u32, opt }, wrap_opt(fty)));
                }
                if let Some((g, st)) = c.accessors.get(prop).cloned() {
                    let ty = match (g, st) {
                        (Some((f, _)), _) => self.func_ret(f)?,
                        (None, Some((f, _))) => self.func_params(f)?.first().map_or(Type::Any, |p| p.ty.clone()),
                        _ => Type::Any,
                    };
                    let ty = self.subst(&ty, &map);
                    return Ok(HExpr::read(
                        Place::Accessor {
                            obj: h,
                            get: g.map(|x| x.1),
                            set: st.map(|x| x.1),
                            opt,
                        },
                        wrap_opt(ty),
                    ));
                }
                if let Some((f, _)) = c.methods.get(prop).cloned() {
                    let ft = self.func_type(f)?;
                    let ty = Type::Func(Rc::new(self.subst_fn(&ft, &map)));
                    return Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, wrap_opt(ty)));
                }
                return err(format!("property '{prop}' does not exist on type '{}'", self.show(&t)), span);
            }
            _ => {}
        }
        // Structural (objects, interfaces, unions of them).
        let mut tys = Vec::new();
        for m in t.members() {
            if m.is_nullish() {
                continue;
            }
            if let Type::Class(..) = m {
                let tmp = HExpr::new(HK::Undefined, m.clone());
                let r = self.member_read(tmp, prop, false, span)?;
                tys.push(r.ty);
                continue;
            }
            if m.is_dynamic() {
                tys.push(Type::Any);
                continue;
            }
            if matches!(m, Type::String | Type::StrLit(_) | Type::Array(_)) && &**prop == "length" {
                tys.push(Type::Number);
                continue;
            }
            let Some(o) = self.obj_view(&m) else {
                return err(format!("property '{prop}' does not exist on type '{}'", self.show(&m)), span);
            };
            match o.prop(prop) {
                Some(p) => tys.push(p.ty.clone()),
                None => match &o.index {
                    Some(it) if t.members().len() == 1 => {
                        // Index signature: dynamic key access.
                        let ty = it.clone();
                        return Ok(HExpr::read(
                            Place::DynIndex {
                                obj: h,
                                key: HExpr::str(prop),
                                opt,
                            },
                            wrap_opt(ty),
                        ));
                    }
                    Some(it) => tys.push(it.clone()),
                    None => {
                        return err(format!("property '{prop}' does not exist on type '{}'", self.show(&m)), span);
                    }
                },
            }
        }
        let ty = Type::union(tys);
        Ok(HExpr::read(Place::Prop { obj: h, name: prop.clone(), opt }, wrap_opt(ty)))
    }

    fn check_index(&mut self, obj: &Expr, index: &Expr, opt: bool, span: Span) -> CResult<HExpr> {
        // Static string-literal key → member access.
        if let ExprKind::Str(s) = &index.kind {
            let h = self.check_expr(obj, None)?;
            let base = h.ty.non_null();
            if !matches!(base, Type::Any) && self.obj_view(&base).is_some_and(|o| o.prop(s).is_some()) {
                return self.member_read(h, s, opt, span);
            }
            let hi = self.check_expr(index, None)?;
            return self.index_read_opt(h, hi, opt, span);
        }
        let h = self.check_expr(obj, None)?;
        let hi = self.check_expr(index, None)?;
        self.index_read_opt(h, hi, opt, span)
    }

    pub fn index_read(&mut self, h: HExpr, hi: HExpr, span: Span) -> CResult<HExpr> {
        self.index_read_opt(h, hi, false, span)
    }

    fn index_read_opt(&mut self, h: HExpr, hi: HExpr, opt: bool, span: Span) -> CResult<HExpr> {
        let t = self.norm(&h.ty.non_null());
        // Generic indexing (`obj[k]` with `k: K extends keyof T`): `T[K]`.
        if matches!(t, Type::Param(_) | Type::Deferred(_)) || matches!(self.norm(&hi.ty), Type::Param(_)) {
            let ty = self.norm(&Type::Deferred(Rc::new(Deferred::Index(t.clone(), hi.ty.clone()))));
            let key = hi.cast(Type::Any);
            return Ok(HExpr::read(Place::DynIndex { obj: h.cast(Type::Any), key, opt }, wrap_opt_ty(opt, ty)));
        }
        let wrap_opt = |ty: Type| if opt { Type::union([ty, Type::Undefined]) } else { ty };
        let numeric = hi.ty.is_numeric();
        match &t {
            Type::Array(e) if numeric => {
                let e = (**e).clone();
                Ok(HExpr::read(Place::Index { obj: h, index: hi, opt }, wrap_opt(e)))
            }
            Type::Tuple(ts) if numeric => {
                let ty = match hi.kind {
                    HK::Num(i) if (i as usize) < ts.len() && i.fract() == 0.0 => ts[i as usize].clone(),
                    _ => Type::union(ts.iter().cloned()),
                };
                Ok(HExpr::read(Place::Index { obj: h, index: hi, opt }, wrap_opt(ty)))
            }
            Type::String | Type::StrLit(_) if numeric => {
                Ok(HExpr::read(Place::Index { obj: h, index: hi, opt }, wrap_opt(Type::String)))
            }
            Type::Any => Ok(HExpr::read(Place::DynIndex { obj: h, key: hi, opt }, Type::Any)),
            _ => {
                let o = self.obj_view(&t);
                match o {
                    Some(o) => {
                        let ty = if let HK::Str(s) = &hi.kind {
                            o.prop(s).map(|p| p.ty.clone()).or(o.index.clone())
                        } else {
                            o.index.clone().or_else(|| {
                                // keyof-typed key into a closed object: union of props.
                                if hi.ty.is_stringy() && !o.props.is_empty() {
                                    Some(Type::union(o.props.iter().map(|p| p.ty.clone())))
                                } else {
                                    None
                                }
                            })
                        };
                        let Some(ty) = ty else {
                            return err(format!("type '{}' has no index signature", self.show(&t)), span);
                        };
                        Ok(HExpr::read(Place::DynIndex { obj: h, key: hi, opt }, wrap_opt(ty)))
                    }
                    None => err(format!("type '{}' cannot be indexed by '{}'", self.show(&t), self.show(&hi.ty)), span),
                }
            }
        }
    }

    // ───────────── calls ─────────────

    fn check_call(&mut self, callee: &Expr, targs: &[TypeExpr], args: &[ArrayElem], opt: bool, span: Span) -> CResult<HExpr> {
        let mut explicit = Vec::new();
        for t in targs {
            explicit.push(self.resolve_type(t)?);
        }
        match &callee.kind {
            ExprKind::Super => return self.check_super_call(args, span),
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Member { .. }) => {
                return self.check_call(inner, targs, args, opt, span);
            }
            ExprKind::Member { obj, prop, .. } if opt && !matches!(obj.kind, ExprKind::Super) => {
                // `o.f?.()`: call only if `o.f` isn't nullish.
                match self.check_expr(callee, None) {
                    Ok(f) => return self.value_call(f, None, args, &explicit, opt, span),
                    Err(e) => {
                        // A built-in method (always present): an ordinary call.
                        let recv = self.check_expr(obj, None)?;
                        if self.builtin_method(&recv.ty.non_null(), prop).is_none() {
                            return Err(e);
                        }
                        return self.method_call(recv, prop, false, args, &explicit, callee.span, span);
                    }
                }
            }
            ExprKind::Member { obj, prop, optional } => {
                if let ExprKind::Super = obj.kind {
                    return self.check_super_method(prop, args, span);
                }
                if let ExprKind::Ident(n) = &obj.kind {
                    match self.lookup_val(n) {
                        Some(VB::Builtin(ns)) => {
                            let Some(NsMember::Method(nat, ft)) = self.ns_member(ns, prop) else {
                                return err(format!("'{n}.{prop}' is not a function"), callee.span);
                            };
                            if nat == Native::ArrFrom {
                                return self.check_array_from(args, span);
                            }
                            let (hargs, ret, _) = self.check_args(&ft, args, &explicit, span)?;
                            let ret = match nat {
                                Native::PromiseAll | Native::PromiseAllSettled | Native::PromiseRace | Native::PromiseAny => {
                                    let at = match hargs.first() {
                                        Some(HElem::Expr(h)) => Some(h.ty.clone()),
                                        _ => None,
                                    };
                                    self.promise_combinator_type(nat, at.as_ref())
                                }
                                _ => ret,
                            };
                            return Ok(self.native_call(nat, None, hargs, ret));
                        }
                        Some(VB::Class(cid)) => {
                            self.ensure_class(cid)?;
                            if let Some(VB::Func(v, f)) = self.classes[cid as usize].statics.get(prop).cloned() {
                                return self.direct_call(f, Some(v), None, args, &explicit, span);
                            }
                        }
                        None => {
                            if let Some((nat, ft)) = special_global_call(n, prop) {
                                let (hargs, ret, _) = self.check_args(&ft, args, &explicit, span)?;
                                return Ok(self.native_call(nat, None, hargs, ret));
                            }
                        }
                        _ => {}
                    }
                }
                let recv = self.check_expr(obj, None)?;
                return self.method_call(recv, prop, *optional, args, &explicit, callee.span, span);
            }
            ExprKind::Ident(n) => match self.lookup_val(n) {
                Some(VB::Func(v, f)) => {
                    self.note_var_use(v);
                    return self.direct_call(f, Some(v), None, args, &explicit, span);
                }
                Some(VB::NativeFn(nat)) => {
                    let ft = global_fn_type(nat);
                    let (hargs, ret, _) = self.check_args(&ft, args, &explicit, span)?;
                    return Ok(self.native_call(nat, None, hargs, ret));
                }
                Some(VB::Builtin(ns @ (BuiltinNs::String | BuiltinNs::Number | BuiltinNs::Boolean))) => {
                    let nat = match ns {
                        BuiltinNs::String => Native::StringFn,
                        BuiltinNs::Number => Native::NumberFn,
                        _ => Native::BooleanFn,
                    };
                    let ft = global_fn_type(nat);
                    let (hargs, ret, _) = self.check_args(&ft, args, &explicit, span)?;
                    return Ok(self.native_call(nat, None, hargs, ret));
                }
                Some(VB::Builtin(BuiltinNs::Symbol)) => {
                    let hargs = self.untyped_args(args)?;
                    return Ok(self.native_call(Native::SymbolFn, None, hargs, Type::Symbol));
                }
                Some(VB::Builtin(BuiltinNs::Array)) => {
                    let (hargs, _, _) = self.check_args(&global_fn_type(Native::ArrNew), args, &[], span)?;
                    return Ok(HExpr::new(HK::NewBuiltin(Native::ArrNew, hargs), Type::array(Type::Any)));
                }
                Some(VB::Class(c)) => {
                    return err(
                        format!("class constructor '{}' cannot be invoked without 'new'", self.classes[c as usize].name),
                        span,
                    );
                }
                _ => {}
            },
            _ => {}
        }
        let f = self.check_expr(callee, None)?;
        self.value_call(f, None, args, &explicit, opt, span)
    }

    fn check_array_from(&mut self, args: &[ArrayElem], span: Span) -> CResult<HExpr> {
        let Some(ArrayElem::Expr(src)) = args.first() else {
            return err("Array.from expects an argument", span);
        };
        let hs = self.check_expr(src, None)?;
        let elem = self.iter_elem(&hs.ty).unwrap_or(Type::Any);
        let mut hargs = vec![HElem::Expr(hs)];
        let mut ret = Type::array(elem.clone());
        if let Some(ArrayElem::Expr(m)) = args.get(1) {
            let cb = Type::func(
                vec![
                    FParam {
                        name: "v".into(),
                        ty: elem,
                        optional: false,
                        rest: false,
                    },
                    FParam {
                        name: "i".into(),
                        ty: Type::Number,
                        optional: false,
                        rest: false,
                    },
                ],
                Type::Any,
            );
            let hm = self.check_expr(m, Some(&cb))?;
            if let Type::Func(f) = &hm.ty {
                ret = Type::array(f.ret.widen());
            }
            hargs.push(HElem::Expr(hm));
        }
        Ok(self.native_call(Native::ArrFrom, None, hargs, ret))
    }

    pub fn direct_call(
        &mut self,
        f: FuncId,
        closure: Option<VarId>,
        this: Option<HExpr>,
        args: &[ArrayElem],
        explicit: &[Type],
        span: Span,
    ) -> CResult<HExpr> {
        let params = self.func_params(f)?;
        let ret = self.func_ret(f)?;
        let ft = FuncType {
            tparams: self.funcs[f as usize].tparams.clone(),
            params,
            ret,
        };
        let (hargs, ret, _) = self.check_args(&ft, args, explicit, span)?;
        Ok(HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Direct { func: f, closure },
                this,
                args: hargs,
                param_kinds: None,
                rest_at: None,
                ret_kind: Kind::Val,
                opt: false,
            })),
            ret,
        ))
    }

    fn check_super_call(&mut self, args: &[ArrayElem], span: Span) -> CResult<HExpr> {
        let Some(cid) = self.ctx().class.filter(|_| self.ctx().kind == FnKind::Ctor) else {
            return err("'super()' is only valid in a derived class constructor", span);
        };
        let Some((pid, pargs)) = self.classes[cid as usize].parent.clone() else {
            return err("'super()' is only valid in a derived class constructor", span);
        };
        let pctor = self.classes[pid as usize].ctor;
        let map = self.class_subst(pid, &pargs);
        let params = self.func_params(pctor)?;
        let ft = FuncType {
            tparams: vec![],
            params: params
                .iter()
                .map(|p| FParam {
                    ty: self.subst(&p.ty, &map),
                    ..p.clone()
                })
                .collect(),
            ret: Type::Void,
        };
        let (hargs, _, _) = self.check_args(&ft, args, &[], span)?;
        let (tv, tt) = self.this_binding().unwrap();
        Ok(HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Direct { func: pctor, closure: None },
                this: Some(HExpr::read(Place::Var(tv), tt)),
                args: hargs,
                param_kinds: None,
                rest_at: None,
                ret_kind: Kind::Val,
                opt: false,
            })),
            Type::Void,
        ))
    }

    fn check_super_method(&mut self, prop: &Name, args: &[ArrayElem], span: Span) -> CResult<HExpr> {
        let Some(cid) = self.stack.iter().rev().find(|c| c.kind != FnKind::Arrow).and_then(|c| c.class) else {
            return err("'super' is only valid inside a class", span);
        };
        let Some((pid, pargs)) = self.classes[cid as usize].parent.clone() else {
            return err("class has no parent for 'super'", span);
        };
        self.ensure_class(pid)?;
        let Some((f, _)) = self.classes[pid as usize].methods.get(prop).cloned() else {
            return err(format!("parent class has no method '{prop}'"), span);
        };
        let (tv, tt) = self.this_binding().unwrap();
        self.note_var_use(tv);
        let map = self.class_subst(pid, &pargs);
        let ft = self.func_type(f)?;
        let ft = self.subst_fn(&ft, &map);
        let (hargs, ret, _) = self.check_args(&ft, args, &[], span)?;
        Ok(HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Direct { func: f, closure: None },
                this: Some(HExpr::read(Place::Var(tv), tt)),
                args: hargs,
                param_kinds: None,
                rest_at: None,
                ret_kind: Kind::Val,
                opt: false,
            })),
            ret,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn method_call(
        &mut self,
        recv: HExpr,
        prop: &Name,
        opt: bool,
        args: &[ArrayElem],
        explicit: &[Type],
        cspan: Span,
        span: Span,
    ) -> CResult<HExpr> {
        let t = recv.ty.non_null();
        let wrap = |ty: Type| if opt { Type::union([ty, Type::Undefined]) } else { ty };
        // Built-in methods.
        let builtin_recv = match &t {
            Type::Union(ms) => {
                // e.g. string | string[] — use the common built-in if all agree.
                let ms: Vec<&Type> = ms.iter().filter(|m| !m.is_nullish()).collect();
                if ms.len() == 1 { Some(ms[0].clone()) } else { None }
            }
            t => Some(t.clone()),
        };
        if let Some(bt) = &builtin_recv
            && let Some((nat, ft)) = self.builtin_method(bt, prop)
        {
            // reduce without an initial value: accumulator is the element type.
            let ft = if matches!(nat, Native::ArrReduce | Native::ArrReduceRight) && args.len() == 1 {
                let elem = self.iter_elem(bt).unwrap_or(Type::Any);
                let map: Subst = [(self.tp_u, elem)].into_iter().collect();
                Rc::new(FuncType {
                    tparams: vec![],
                    params: vec![self.subst_fn(&ft, &map).params[0].clone()],
                    ret: self.subst(&ft.ret, &map),
                })
            } else {
                ft
            };
            let (hargs, ret, _) = self.check_args(&ft, args, explicit, span)?;
            let mut c = self.native_call(nat, Some(recv), hargs, wrap(ret));
            if let HK::Call(call) = &mut c.kind {
                call.opt = opt;
            }
            return Ok(c);
        }
        match &t {
            Type::Any => {
                let mut hargs = Vec::new();
                for a in args {
                    hargs.push(match a {
                        ArrayElem::Expr(e) => HElem::Expr(self.check_expr(e, None)?),
                        ArrayElem::Spread(e) => HElem::Spread(self.check_expr(e, None)?),
                        ArrayElem::Hole => HElem::Expr(HExpr::undefined()),
                    });
                }
                return Ok(HExpr::new(
                    HK::Call(Box::new(HCall {
                        callee: Callee::Prop(prop.clone()),
                        this: Some(recv),
                        args: hargs,
                        param_kinds: None,
                        rest_at: None,
                        ret_kind: Kind::Val,
                        opt,
                    })),
                    Type::Any,
                ));
            }
            Type::Class(cid, cargs) => {
                let (cid, cargs) = (*cid, cargs.clone());
                self.ensure_class(cid)?;
                if let Some((f, slot)) = self.classes[cid as usize].methods.get(prop).cloned() {
                    let map = self.class_subst(cid, &cargs);
                    let ft = self.func_type(f)?;
                    let ft = self.subst_fn(&ft, &map);
                    let (hargs, ret, _) = self.check_args(&ft, args, explicit, span)?;
                    let kinds = self.arrival_kinds(f)?;
                    let rest_at = ft.params.iter().position(|p| p.rest);
                    return Ok(HExpr::new(
                        HK::Call(Box::new(HCall {
                            callee: Callee::Method { class: cid, vslot: slot },
                            this: Some(recv),
                            args: hargs,
                            param_kinds: Some(kinds),
                            rest_at,
                            ret_kind: Kind::Val,
                            opt,
                        })),
                        wrap(ret),
                    ));
                }
            }
            Type::ClassCtor(cid) => {
                let cid = *cid;
                self.ensure_class(cid)?;
                if let Some(VB::Func(v, f)) = self.classes[cid as usize].statics.get(prop).cloned() {
                    return self.direct_call(f, Some(v), None, args, explicit, span);
                }
            }
            _ => {}
        }
        // A function-valued property: call with `this` = receiver.
        let member = self.member_read(recv.clone(), prop, opt, cspan)?;
        let fty = member.ty.non_null();
        let ft = match &fty {
            Type::Func(f) => Some(f.clone()),
            Type::Any => None,
            t => match self.obj_view(t).and_then(|o| o.call.clone()) {
                Some(c) => Some(c),
                None => return err(format!("'{prop}' is not callable (type '{}')", self.show(&member.ty)), cspan),
            },
        };
        let (hargs, ret) = match &ft {
            Some(f) => {
                let (a, r, _) = self.check_args(f, args, explicit, span)?;
                (a, r)
            }
            None => (self.untyped_args(args)?, Type::Any),
        };
        let param_kinds = ft
            .as_ref()
            .map(|f| f.params.iter().take_while(|p| !p.rest).map(param_arrival).collect());
        Ok(HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Prop(prop.clone()),
                this: Some(recv),
                args: hargs,
                param_kinds,
                rest_at: None,
                ret_kind: Kind::Val,
                opt,
            })),
            wrap(ret),
        ))
    }

    fn untyped_args(&mut self, args: &[ArrayElem]) -> CResult<Vec<HElem>> {
        let mut out = Vec::new();
        for a in args {
            out.push(match a {
                ArrayElem::Expr(e) => HElem::Expr(self.check_expr(e, None)?),
                ArrayElem::Spread(e) => HElem::Spread(self.check_expr(e, None)?),
                ArrayElem::Hole => HElem::Expr(HExpr::undefined()),
            });
        }
        Ok(out)
    }

    fn value_call(&mut self, f: HExpr, this: Option<HExpr>, args: &[ArrayElem], explicit: &[Type], opt: bool, span: Span) -> CResult<HExpr> {
        let fty = f.ty.non_null();
        let ft = match &fty {
            Type::Func(ft) => Some(ft.clone()),
            Type::Any | Type::Never => None,
            t => match self.obj_view(t).and_then(|o| o.call.clone()) {
                Some(c) => Some(c),
                None => return err(format!("type '{}' is not callable", self.show(&f.ty)), span),
            },
        };
        let (hargs, ret) = match &ft {
            Some(ft) => {
                let (a, r, _) = self.check_args(ft, args, explicit, span)?;
                (a, r)
            }
            None => (self.untyped_args(args)?, Type::Any),
        };
        let param_kinds = ft
            .as_ref()
            .map(|f| f.params.iter().take_while(|p| !p.rest).map(param_arrival).collect());
        let ret = if opt { Type::union([ret, Type::Undefined]) } else { ret };
        Ok(HExpr::new(
            HK::Call(Box::new(HCall {
                callee: Callee::Value(f),
                this,
                args: hargs,
                param_kinds,
                rest_at: None,
                ret_kind: Kind::Val,
                opt,
            })),
            ret,
        ))
    }

    /// Check arguments against a signature, inferring type parameters.
    /// Returns (args, instantiated return type, substitution).
    pub fn check_args(&mut self, ft: &FuncType, args: &[ArrayElem], explicit: &[Type], span: Span) -> CResult<(Vec<HElem>, Type, Subst)> {
        let mut subst: Subst = Subst::default();
        for (i, tp) in ft.tparams.iter().enumerate() {
            if let Some(t) = explicit.get(i) {
                subst.insert(*tp, t.clone());
            }
        }
        let rest_idx = ft.params.iter().position(|p| p.rest);
        let has_spread = args.iter().any(|a| matches!(a, ArrayElem::Spread(_)));
        // Arity.
        let min = ft.min_args();
        if !has_spread && args.len() < min {
            return err(format!("expected {} argument{}, but got {}", min, if min == 1 { "" } else { "s" }, args.len()), span);
        }
        if rest_idx.is_none() && !has_spread && args.len() > ft.params.len() {
            return err(
                format!("expected {} argument{}, but got {}", ft.params.len(), if ft.params.len() == 1 { "" } else { "s" }, args.len()),
                span,
            );
        }
        let param_for = |i: usize| -> Option<(Type, bool)> {
            match rest_idx {
                Some(r) if i >= r => Some((
                    match &ft.params[r].ty {
                        Type::Array(e) => (**e).clone(),
                        t => t.clone(),
                    },
                    true,
                )),
                _ => ft.params.get(i).map(|p| (p.ty.clone(), false)),
            }
        };
        let mut out: Vec<Option<HElem>> = vec![None; args.len()];
        // Two passes: context-insensitive arguments first (so callbacks see inferred types).
        for pass in 0..2 {
            for (i, a) in args.iter().enumerate() {
                let is_fn = matches!(a, ArrayElem::Expr(e) if is_fn_expr(e));
                if (pass == 0) == is_fn {
                    continue;
                }
                match a {
                    ArrayElem::Spread(e) => {
                        let h = self.check_expr(e, None)?;
                        if self.iter_elem(&h.ty).is_none() {
                            return err(format!("type '{}' is not iterable", self.show(&h.ty)), e.span);
                        }
                        out[i] = Some(HElem::Spread(h));
                    }
                    ArrayElem::Hole => out[i] = Some(HElem::Expr(HExpr::undefined())),
                    ArrayElem::Expr(e) => {
                        let pty = param_for(i).map(|(t, _)| t).unwrap_or(Type::Any);
                        let ctx = erase_unbound(&self.subst(&pty, &subst), &ft.tparams);
                        let h = self.check_expr(e, Some(&ctx))?;
                        self.infer(&pty, &h.ty, &ft.tparams, &mut subst);
                        out[i] = Some(HElem::Expr(h));
                    }
                }
            }
        }
        // Assignability against the instantiated parameter types.
        let mut hargs = Vec::new();
        for (i, (a, h)) in args.iter().zip(out).enumerate() {
            let h = h.unwrap();
            if let (ArrayElem::Expr(e), HElem::Expr(he)) = (a, &h)
                && let Some((pty, _)) = param_for(i)
            {
                let pty = erase_unbound(&self.subst(&pty, &subst), &ft.tparams);
                if !self.assignable(&he.ty, &pty) {
                    return err(
                        format!(
                            "argument of type '{}' is not assignable to parameter of type '{}'",
                            self.show(&he.ty),
                            self.show(&pty)
                        ),
                        e.span,
                    );
                }
            }
            hargs.push(h);
        }
        let ret = erase_unbound(&self.subst(&ft.ret, &subst), &ft.tparams);
        Ok((hargs, ret, subst))
    }

    /// Infer type-parameter bindings from an argument type.
    pub fn infer(&mut self, p: &Type, a: &Type, tps: &[TParamId], subst: &mut Subst) {
        match (p, a) {
            (Type::Param(id), _) if tps.contains(id) => {
                if matches!(a, Type::Never) {
                    return;
                }
                match subst.get(id) {
                    None => {
                        subst.insert(*id, a.widen());
                    }
                    Some(Type::Any) if !a.is_dynamic() => {
                        subst.insert(*id, a.widen());
                    }
                    Some(prev) => {
                        let prev = prev.clone();
                        if !self.assignable(a, &prev) && self.assignable(&prev, a) {
                            subst.insert(*id, a.widen());
                        }
                    }
                }
            }
            (Type::Union(ps), _) => {
                let params: Vec<&Type> = ps.iter().filter(|t| contains_param(t)).collect();
                if params.len() == 1 {
                    let others: Vec<Type> = ps.iter().filter(|t| !contains_param(t)).cloned().collect();
                    let rest = Type::union(a.members().into_iter().filter(|m| !others.contains(m)));
                    self.infer(params[0], &rest, tps, subst);
                }
            }
            (Type::Array(pe), Type::Array(ae)) => self.infer(&pe.clone(), &ae.clone(), tps, subst),
            (Type::Array(pe), Type::Tuple(ts)) => {
                let u = Type::union(ts.iter().cloned());
                self.infer(&pe.clone(), &u, tps, subst)
            }
            (Type::Tuple(ps), Type::Tuple(as_)) => {
                for (x, y) in ps.clone().iter().zip(as_.clone().iter()) {
                    self.infer(x, y, tps, subst);
                }
            }
            (Type::Func(pf), Type::Func(af)) => {
                for (x, y) in pf.params.iter().zip(af.params.iter()) {
                    self.infer(&x.ty, &y.ty, tps, subst);
                }
                if !matches!(af.ret, Type::Void) || matches!(pf.ret, Type::Param(_)) {
                    self.infer(&pf.ret, &af.ret, tps, subst);
                }
            }
            (Type::Map(pk, pv), Type::Map(ak, av)) => {
                self.infer(&pk.clone(), &ak.clone(), tps, subst);
                self.infer(&pv.clone(), &av.clone(), tps, subst);
            }
            (Type::Set(pk), Type::Set(ak)) => self.infer(&pk.clone(), &ak.clone(), tps, subst),
            (Type::Class(pc, pa), Type::Class(ac, aa)) if pc == ac => {
                for (x, y) in pa.clone().iter().zip(aa.clone().iter()) {
                    self.infer(x, y, tps, subst);
                }
            }
            (Type::Iface(pc, pa), Type::Iface(ac, aa)) if pc == ac => {
                for (x, y) in pa.clone().iter().zip(aa.clone().iter()) {
                    self.infer(x, y, tps, subst);
                }
            }
            (Type::Object(_) | Type::Iface(..), _) if contains_param(p) => {
                if let (Some(po), Some(ao)) = (self.obj_view(p), self.obj_view(a)) {
                    for pp in &po.props {
                        if let Some(ap) = ao.prop(&pp.name) {
                            self.infer(&pp.ty, &ap.ty, tps, subst);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn check_new(&mut self, callee: &Expr, targs: &[TypeExpr], args: &[ArrayElem], want: Option<&Type>, span: Span) -> CResult<HExpr> {
        let mut explicit = Vec::new();
        for t in targs {
            explicit.push(self.resolve_type(t)?);
        }
        // A class held in a variable or reached through an expression:
        // statically a known class, so construct it directly.
        let as_class = match &callee.kind {
            ExprKind::Ident(n) => !matches!(self.lookup_val(n), Some(VB::Var(_)) | None),
            _ => false,
        };
        if !as_class {
            let h = self.check_expr(callee, None)?;
            if let Type::ClassCtor(cid) = h.ty {
                let hidden: Name = format!("\u{1}class{cid}").into();
                self.module.vals.borrow_mut().insert(hidden.clone(), VB::Class(cid));
                let alias = Expr {
                    kind: ExprKind::Ident(hidden),
                    span: callee.span,
                };
                return self.check_new(&alias, targs, args, want, span);
            }
            if let ExprKind::Ident(n) = &callee.kind
                && self.lookup_val(n).is_none()
            {
                return err(format!("'{n}' is not a constructor (or is not supported yet)"), callee.span);
            }
            return err(format!("'new' needs a class; this expression has type '{}'", self.show(&h.ty)), callee.span);
        }
        let ExprKind::Ident(n) = &callee.kind else { unreachable!() };
        match self.lookup_val(n) {
            Some(VB::Class(cid)) => {
                self.ensure_class(cid)?;
                if self.classes[cid as usize].is_abstract {
                    return err(format!("cannot create an instance of abstract class '{n}'"), span);
                }
                let ctor = self.classes[cid as usize].ctor;
                let tps = self.classes[cid as usize].tparams.clone();
                let params = self.func_params(ctor)?;
                let ft = FuncType {
                    tparams: tps.clone(),
                    params,
                    ret: Type::Void,
                };
                // Contextual type arguments: `const b: Box<number> = new Box(...)`.
                let mut ex = explicit.clone();
                if ex.is_empty()
                    && let Some(Type::Class(wc, wa)) = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Class(..))))
                    && wc == cid
                {
                    ex = wa.to_vec();
                }
                let (hargs, _, subst) = self.check_args(&ft, args, &ex, span)?;
                let targs: Vec<Type> = tps.iter().map(|t| subst.get(t).cloned().unwrap_or(Type::Any)).collect();
                Ok(HExpr::new(HK::New { class: cid, args: hargs }, Type::Class(cid, targs.into())))
            }
            Some(VB::Builtin(BuiltinNs::RegExp)) => {
                let hargs = self.untyped_args(args)?;
                Ok(HExpr::new(HK::NewBuiltin(Native::RegexNew, hargs), Type::RegExp))
            }
            Some(VB::Builtin(BuiltinNs::Promise)) => {
                let mut t = explicit.first().cloned();
                if t.is_none()
                    && let Some(Type::Promise(w)) = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Promise(_))))
                {
                    t = Some((*w).clone());
                }
                let t = t.unwrap_or(Type::Any);
                let fnt = |params: Vec<FParam>| Type::func(params, Type::Void);
                let fp = |name: &str, ty: Type, optional: bool| FParam {
                    name: name.into(),
                    ty,
                    optional,
                    rest: false,
                };
                let resolve = fnt(vec![fp("value", Type::union([t.clone(), Type::promise(t.clone())]), true)]);
                let reject = fnt(vec![fp("reason", Type::Any, true)]);
                let exec = Rc::new(FuncType {
                    tparams: vec![],
                    params: vec![fp("executor", fnt(vec![fp("resolve", resolve, false), fp("reject", reject, false)]), false)],
                    ret: Type::Void,
                });
                let (hargs, _, _) = self.check_args(&exec, args, &[], span)?;
                Ok(HExpr::new(HK::NewBuiltin(Native::PromiseNew, hargs), Type::promise(t)))
            }
            Some(VB::Builtin(BuiltinNs::MapCtor)) => {
                let (mut k, mut v) = (explicit.first().cloned(), explicit.get(1).cloned());
                if k.is_none()
                    && let Some(Type::Map(wk, wv)) = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Map(..))))
                {
                    k = Some((*wk).clone());
                    v = Some((*wv).clone());
                }
                let hargs = self.untyped_args(args)?;
                if k.is_none()
                    && let Some(HElem::Expr(h)) = hargs.first()
                    && let Some(Type::Tuple(ts)) = self.iter_elem(&h.ty)
                    && ts.len() == 2
                {
                    k = Some(ts[0].widen());
                    v = Some(ts[1].widen());
                }
                let ty = Type::Map(Rc::new(k.unwrap_or(Type::Any)), Rc::new(v.unwrap_or(Type::Any)));
                Ok(HExpr::new(HK::NewBuiltin(Native::MapNew, hargs), ty))
            }
            Some(VB::Builtin(BuiltinNs::SetCtor)) => {
                let mut t = explicit.first().cloned();
                if t.is_none()
                    && let Some(Type::Set(wt)) = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Set(..))))
                {
                    t = Some((*wt).clone());
                }
                let hargs = self.untyped_args(args)?;
                if t.is_none()
                    && let Some(HElem::Expr(h)) = hargs.first()
                {
                    t = self.iter_elem(&h.ty).map(|e| e.widen());
                }
                let ty = Type::Set(Rc::new(t.unwrap_or(Type::Any)));
                Ok(HExpr::new(HK::NewBuiltin(Native::SetNew, hargs), ty))
            }
            Some(VB::Builtin(BuiltinNs::Array)) => {
                let mut t = explicit.first().cloned();
                if t.is_none()
                    && let Some(Type::Array(wt)) = want.map(|w| self.pick_member(w, |t| matches!(t, Type::Array(..))))
                {
                    t = Some((*wt).clone());
                }
                let hargs = self.untyped_args(args)?;
                Ok(HExpr::new(HK::NewBuiltin(Native::ArrNew, hargs), Type::array(t.unwrap_or(Type::Any))))
            }
            _ => err(format!("'{n}' is not a constructor (or is not supported yet)"), callee.span),
        }
    }

    // ───────────── narrowing helpers used by expressions ─────────────

    /// Narrowing key for an expression path (`x`, `this`, `x.a.b`).
    pub fn narrow_key(&self, e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Ident(n) => match self.lookup_val(n) {
                Some(VB::Var(v)) => Some(format!("{v}")),
                _ => None,
            },
            ExprKind::This => self.this_binding().map(|(v, _)| format!("{v}")),
            ExprKind::Member { obj, prop, optional: false } => self.narrow_key_member(obj, prop),
            ExprKind::Paren(x) | ExprKind::NonNull(x) => self.narrow_key(x),
            _ => None,
        }
    }

    pub fn narrow_key_member(&self, obj: &Expr, prop: &Name) -> Option<String> {
        self.narrow_key(obj).map(|k| format!("{k}.{prop}"))
    }

    /// Drop narrowings invalidated by an assignment to `target`.
    pub fn invalidate(&mut self, target: &Expr) {
        if let Some(k) = self.narrow_key(target) {
            let prefix = format!("{k}.");
            self.ctx_mut().narrow.retain(|key, _| key != &k && !key.starts_with(&prefix));
        }
    }
}

fn wrap_opt_ty(opt: bool, ty: Type) -> Type {
    if opt { Type::union([ty, Type::Undefined]) } else { ty }
}

pub fn contains_param(t: &Type) -> bool {
    match t {
        Type::Param(_) => true,
        Type::Array(e) | Type::Set(e) => contains_param(e),
        Type::Tuple(ts) | Type::Union(ts) | Type::Class(_, ts) | Type::Iface(_, ts) => ts.iter().any(contains_param),
        Type::Map(k, v) => contains_param(k) || contains_param(v),
        Type::Func(f) => f.params.iter().any(|p| contains_param(&p.ty)) || contains_param(&f.ret),
        Type::Object(o) => o.props.iter().any(|p| contains_param(&p.ty)),
        Type::Promise(t) => contains_param(t),
        Type::Iter(it) => contains_param(&it.yield_ty) || contains_param(&it.return_ty),
        Type::Deferred(_) => true,
        _ => false,
    }
}

/// Replace still-unbound type parameters of a call with `any`.
fn erase_unbound(t: &Type, tps: &[TParamId]) -> Type {
    match t {
        Type::Param(id) if tps.contains(id) => Type::Any,
        Type::Array(e) => Type::array(erase_unbound(e, tps)),
        Type::Set(e) => Type::Set(Rc::new(erase_unbound(e, tps))),
        Type::Map(k, v) => Type::Map(Rc::new(erase_unbound(k, tps)), Rc::new(erase_unbound(v, tps))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| erase_unbound(x, tps)).collect()),
        Type::Union(ts) => Type::union(ts.iter().map(|x| erase_unbound(x, tps))),
        Type::Class(c, ts) => Type::Class(*c, ts.iter().map(|x| erase_unbound(x, tps)).collect()),
        Type::Iface(c, ts) => Type::Iface(*c, ts.iter().map(|x| erase_unbound(x, tps)).collect()),
        Type::Func(f) => Type::Func(Rc::new(FuncType {
            tparams: f.tparams.clone(),
            params: f
                .params
                .iter()
                .map(|p| FParam {
                    ty: erase_unbound(&p.ty, tps),
                    ..p.clone()
                })
                .collect(),
            ret: erase_unbound(&f.ret, tps),
        })),
        Type::Object(o) => Type::Object(Rc::new(ObjType {
            props: o
                .props
                .iter()
                .map(|p| Prop {
                    ty: erase_unbound(&p.ty, tps),
                    ..p.clone()
                })
                .collect(),
            index: o.index.as_ref().map(|t| erase_unbound(t, tps)),
            call: o.call.clone(),
        })),
        t => t.clone(),
    }
}

pub fn truthy_part(t: &Type) -> Type {
    Type::union(t.members().into_iter().filter_map(|m| match m {
        Type::Null | Type::Undefined | Type::Void | Type::BoolLit(false) => None,
        Type::Boolean => Some(Type::BoolLit(true)),
        m => Some(m),
    }))
}

pub fn falsy_part(t: &Type) -> Type {
    Type::union(t.members().into_iter().filter_map(|m| match m {
        Type::Null | Type::Undefined | Type::Void | Type::BoolLit(false) => Some(m),
        Type::Boolean => Some(Type::BoolLit(false)),
        Type::Number | Type::NumLit(_) | Type::Enum(_) => Some(Type::Number),
        Type::BigInt => Some(Type::BigInt),
        Type::String | Type::StrLit(_) => Some(Type::String),
        Type::Any | Type::Unknown | Type::Param(_) => Some(m),
        _ => None,
    }))
}

#[allow(dead_code)]
fn _u(_: &Callee) {}
