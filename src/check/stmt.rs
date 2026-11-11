//! Statements, declarations, destructuring and control-flow narrowing.

use std::rc::Rc;

use rustc_hash::FxHashSet;

use super::*;
use crate::ast::{BinOp, Expr, ExprKind, Pattern, Stmt, StmtKind, UnaryOp, VarKind};

/// Does this statement list always exit (return/throw/break/continue)?
pub fn block_terminates(b: &[Stmt]) -> bool {
    b.iter().any(stmt_terminates)
}

pub fn stmt_terminates(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Return(_) | StmtKind::Throw(_) | StmtKind::Break(_) | StmtKind::Continue(_) => true,
        StmtKind::Block(b) => block_terminates(b),
        StmtKind::If(_, t, Some(e)) => stmt_terminates(t) && stmt_terminates(e),
        StmtKind::Try {
            block,
            handler,
            finalizer,
            ..
        } => {
            finalizer.as_ref().is_some_and(|f| block_terminates(f))
                || (block_terminates(block) && handler.as_ref().is_none_or(|h| block_terminates(h)))
        }
        StmtKind::While(c, body) => matches!(c.kind, ExprKind::Bool(true)) && !contains_break(body),
        StmtKind::For { test: None, body, .. } => !contains_break(body),
        StmtKind::Switch(_, cases) => {
            cases.iter().any(|c| c.test.is_none())
                && cases.iter().all(|c| c.body.is_empty() || (block_terminates(&c.body) && !c.body.iter().any(contains_break)))
        }
        _ => false,
    }
}

fn contains_break(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Break(_) => true,
        StmtKind::Block(b) => b.iter().any(contains_break),
        StmtKind::If(_, t, e) => contains_break(t) || e.as_ref().is_some_and(|e| contains_break(e)),
        StmtKind::Labeled(_, b) => contains_break(b),
        StmtKind::Try {
            block,
            handler,
            finalizer,
            ..
        } => {
            block.iter().any(contains_break)
                || handler.as_ref().is_some_and(|h| h.iter().any(contains_break))
                || finalizer.as_ref().is_some_and(|h| h.iter().any(contains_break))
        }
        // Breaks inside nested loops/switches target those.
        _ => false,
    }
}

/// Names assigned anywhere in a statement (for loop narrowing invalidation).
fn assigned_names(s: &Stmt, out: &mut FxHashSet<Name>) {
    fn expr(e: &Expr, out: &mut FxHashSet<Name>) {
        match &e.kind {
            ExprKind::Assign(_, t, v) => {
                root(t, out);
                expr(t, out);
                expr(v, out);
            }
            ExprKind::Update { target, .. } => root(target, out),
            ExprKind::Binary(_, a, b) | ExprKind::Logical(_, a, b) => {
                expr(a, out);
                expr(b, out);
            }
            ExprKind::Cond(a, b, c) => {
                expr(a, out);
                expr(b, out);
                expr(c, out);
            }
            ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args, .. } => {
                expr(callee, out);
                for a in args {
                    if let crate::ast::ArrayElem::Expr(x) | crate::ast::ArrayElem::Spread(x) = a {
                        expr(x, out);
                    }
                }
            }
            ExprKind::Member { obj, .. } => expr(obj, out),
            ExprKind::Index { obj, index, .. } => {
                expr(obj, out);
                expr(index, out);
            }
            ExprKind::Unary(_, x)
            | ExprKind::Paren(x)
            | ExprKind::NonNull(x)
            | ExprKind::OptChain(x)
            | ExprKind::As(x, _)
            | ExprKind::Satisfies(x, _) => expr(x, out),
            ExprKind::Seq(es) => es.iter().for_each(|x| expr(x, out)),
            ExprKind::Array(es) => {
                for a in es {
                    if let crate::ast::ArrayElem::Expr(x) | crate::ast::ArrayElem::Spread(x) = a {
                        expr(x, out);
                    }
                }
            }
            ExprKind::Template { exprs, .. } => exprs.iter().for_each(|x| expr(x, out)),
            _ => {}
        }
    }
    fn root(e: &Expr, out: &mut FxHashSet<Name>) {
        match &e.kind {
            ExprKind::Ident(n) => {
                out.insert(n.clone());
            }
            ExprKind::This => {
                out.insert("this".into());
            }
            ExprKind::Member { obj, .. } | ExprKind::Index { obj, .. } => root(obj, out),
            ExprKind::Paren(x) | ExprKind::NonNull(x) => root(x, out),
            ExprKind::Array(es) => {
                for a in es {
                    if let crate::ast::ArrayElem::Expr(x) = a {
                        root(x, out);
                    }
                }
            }
            _ => {}
        }
    }
    match &s.kind {
        StmtKind::Expr(e) | StmtKind::Throw(e) => expr(e, out),
        StmtKind::Return(Some(e)) => expr(e, out),
        StmtKind::Var { decls, .. } => {
            for d in decls {
                if let Some(i) = &d.init {
                    expr(i, out);
                }
            }
        }
        StmtKind::Block(b) => b.iter().for_each(|s| assigned_names(s, out)),
        StmtKind::If(c, t, e) => {
            expr(c, out);
            assigned_names(t, out);
            if let Some(e) = e {
                assigned_names(e, out);
            }
        }
        StmtKind::While(c, b) | StmtKind::DoWhile(b, c) => {
            expr(c, out);
            assigned_names(b, out);
        }
        StmtKind::For { init, test, update, body } => {
            if let Some(i) = init {
                assigned_names(i, out);
            }
            if let Some(t) = test {
                expr(t, out);
            }
            if let Some(u) = update {
                expr(u, out);
            }
            assigned_names(body, out);
        }
        StmtKind::ForOf { body, iter, .. } | StmtKind::ForIn { body, obj: iter, .. } => {
            expr(iter, out);
            assigned_names(body, out);
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
            ..
        } => {
            block.iter().for_each(|s| assigned_names(s, out));
            if let Some(h) = handler {
                h.iter().for_each(|s| assigned_names(s, out));
            }
            if let Some(f) = finalizer {
                f.iter().for_each(|s| assigned_names(s, out));
            }
        }
        StmtKind::Switch(d, cases) => {
            expr(d, out);
            for c in cases {
                c.body.iter().for_each(|s| assigned_names(s, out));
            }
        }
        StmtKind::Labeled(_, b) => assigned_names(b, out),
        _ => {}
    }
}

fn pattern_names(p: &Pattern, out: &mut Vec<(Name, Span)>) {
    match p {
        Pattern::Ident(n, s) => out.push((n.clone(), *s)),
        Pattern::Object { props, rest, .. } => {
            for (_, p, _) in props {
                pattern_names(p, out);
            }
            if let Some(r) = rest {
                pattern_names(r, out);
            }
        }
        Pattern::Array { elems, rest, .. } => {
            for (p, _) in elems.iter().flatten() {
                pattern_names(p, out);
            }
            if let Some(r) = rest {
                pattern_names(r, out);
            }
        }
    }
}

impl Checker {
    /// Check a statement list in `scope` (already created). Hoists declarations first.
    pub fn check_block_in(&mut self, stmts: &[Stmt], scope: Rc<Scope>) -> CResult<Vec<HStmt>> {
        let saved = self.ctx().scope.clone();
        self.ctx_mut().scope = scope.clone();
        let r = self.check_block_inner(stmts, &scope);
        self.ctx_mut().scope = saved;
        r
    }

    fn check_block_inner(&mut self, stmts: &[Stmt], scope: &Rc<Scope>) -> CResult<Vec<HStmt>> {
        let mut out = Vec::new();
        let mut funcs: Vec<FuncId> = Vec::new();
        let mut classes: Vec<ClassId> = Vec::new();
        // Pass 1: types.
        for s in stmts {
            match &s.kind {
                StmtKind::Class(c) => {
                    let Some(name) = c.name.clone() else {
                        return err("class declarations need a name", s.span);
                    };
                    let cid = self.declare_class(&name, c.clone(), scope)?;
                    classes.push(cid);
                }
                StmtKind::Interface {
                    name,
                    tparams,
                    extends,
                    members,
                } => {
                    // Declaration merging: append to an existing interface in this scope.
                    let existing = scope.types.borrow().get(name).cloned();
                    if let Some(TB::Iface(id)) = existing {
                        self.ifaces[id as usize].members.extend(members.iter().cloned());
                        self.ifaces[id as usize].extends.extend(extends.iter().cloned());
                        continue;
                    }
                    let is = Scope::new(Some(scope.clone()), scope.owner);
                    let mut tps = Vec::new();
                    for tp in tparams {
                        let id = self.new_tparam(&tp.name, None);
                        is.types.borrow_mut().insert(tp.name.clone(), TB::TParam(id));
                        tps.push(id);
                    }
                    self.ifaces.push(IfaceDef {
                        name: name.clone(),
                        tparams: tps,
                        scope: is,
                        extends: extends.clone(),
                        members: members.clone(),
                        obj: None,
                        resolving: false,
                    });
                    scope.types.borrow_mut().insert(name.clone(), TB::Iface(self.ifaces.len() as u32 - 1));
                }
                StmtKind::TypeAlias { name, tparams, ty } => {
                    let ts = Scope::new(Some(scope.clone()), scope.owner);
                    let mut tps = Vec::new();
                    for tp in tparams {
                        let id = self.new_tparam(&tp.name, None);
                        ts.types.borrow_mut().insert(tp.name.clone(), TB::TParam(id));
                        tps.push(id);
                    }
                    // Object-literal aliases behave like interfaces (allows recursion).
                    if let TypeExpr::Object(members) = ty {
                        self.ifaces.push(IfaceDef {
                            name: name.clone(),
                            tparams: tps,
                            scope: ts,
                            extends: vec![],
                            members: members.clone(),
                            obj: None,
                            resolving: false,
                        });
                        scope.types.borrow_mut().insert(name.clone(), TB::Iface(self.ifaces.len() as u32 - 1));
                    } else {
                        self.aliases.push(AliasDef {
                            name: name.clone(),
                            tparams: tps,
                            scope: ts,
                            ty: ty.clone(),
                            resolved: None,
                            resolving: false,
                        });
                        scope.types.borrow_mut().insert(name.clone(), TB::Alias(self.aliases.len() as u32 - 1));
                    }
                }
                StmtKind::Enum { name, members } => {
                    let mut ms = Vec::new();
                    let mut next = 0.0;
                    for (m, init, sp) in members {
                        let v = match init {
                            None => EnumVal::Num(next),
                            Some(e) => match self.const_eval(e, &ms) {
                                Some(v) => v,
                                None => return err("enum initializers must be constant", *sp),
                            },
                        };
                        if let EnumVal::Num(n) = v {
                            next = n + 1.0;
                        }
                        ms.push((m.clone(), v));
                    }
                    self.enums.push(EnumDef {
                        name: name.clone(),
                        members: ms,
                    });
                    let eid = self.enums.len() as u32 - 1;
                    scope.types.borrow_mut().insert(name.clone(), TB::Enum(eid));
                    scope.vals.borrow_mut().insert(name.clone(), VB::Enum(eid));
                }
                _ => {}
            }
        }
        // Pass 2: values (functions and variables).
        for s in stmts {
            match &s.kind {
                StmtKind::Func(f) => {
                    let name = f.name.clone().unwrap();
                    if f.body.is_none() {
                        // Overload signature or `declare function`.
                        let has_impl = stmts.iter().any(|o| matches!(&o.kind, StmtKind::Func(g) if g.name == f.name && g.body.is_some()));
                        if has_impl || scope.vals.borrow().contains_key(&name) {
                            continue;
                        }
                        return err(format!("function '{name}' has no implementation (ambient declarations are not supported)"), f.span);
                    }
                    let v = self.new_var(name.clone(), None, true, scope);
                    let fid = self.new_func(name.clone(), Some(f.clone()), FnKind::Normal, None, None, scope.clone(), None);
                    scope.vals.borrow_mut().insert(name, VB::Func(v, fid));
                    funcs.push(fid);
                    out.push(HStmt::MakeClosure(v, fid));
                }
                StmtKind::Var { kind, decls, .. } => {
                    let target = if *kind == VarKind::Var { self.ctx().fn_scope.clone() } else { scope.clone() };
                    for d in decls {
                        let mut names = Vec::new();
                        pattern_names(&d.pat, &mut names);
                        for (n, sp) in names {
                            if *kind != VarKind::Var && target.vals.borrow().contains_key(&n) {
                                if let Some(VB::Var(_)) = target.vals.borrow().get(&n) {
                                    return err(format!("cannot redeclare block-scoped variable '{n}'"), sp);
                                }
                            }
                            if *kind == VarKind::Var && matches!(target.vals.borrow().get(&n), Some(VB::Var(_))) {
                                continue;
                            }
                            let ty = match (&d.pat, &d.ty) {
                                (Pattern::Ident(..), Some(t)) => Some(self.resolve_type(t)?),
                                _ => None,
                            };
                            let v = self.new_var(n.clone(), ty, *kind == VarKind::Const, &target);
                            target.vals.borrow_mut().insert(n, VB::Var(v));
                        }
                    }
                }
                _ => {}
            }
        }
        // Pass 3: statements in order, with flow narrowing.
        for s in stmts {
            let hs = self.check_stmt(s, None)?;
            out.extend(hs);
            // After `if (c) return;` the rest of the block sees `!c`.
            if let StmtKind::If(c, t, None) = &s.kind
                && stmt_terminates(t)
            {
                let r = self.refine(c, false)?;
                self.apply(&r);
            }
            if let StmtKind::If(c, t, Some(e)) = &s.kind {
                if stmt_terminates(t) && !stmt_terminates(e) {
                    let r = self.refine(c, false)?;
                    self.apply(&r);
                } else if stmt_terminates(e) && !stmt_terminates(t) {
                    let r = self.refine(c, true)?;
                    self.apply(&r);
                }
            }
        }
        for f in funcs {
            self.ensure_checked(f)?;
        }
        for c in classes {
            self.ensure_class(c)?;
            let ms: Vec<FuncId> = {
                let cd = &self.classes[c as usize];
                let mut v: Vec<FuncId> = cd.vtable.iter().map(|(_, f)| *f).collect();
                v.push(cd.ctor);
                v.extend(cd.statics.values().filter_map(|b| match b {
                    VB::Func(_, f) => Some(*f),
                    _ => None,
                }));
                v
            };
            for f in ms {
                self.ensure_checked(f)?;
            }
        }
        Ok(out)
    }

    fn const_eval(&self, e: &Expr, prev: &[(Name, EnumVal)]) -> Option<EnumVal> {
        Some(match &e.kind {
            ExprKind::Num(n) => EnumVal::Num(*n),
            ExprKind::Str(s) => EnumVal::Str(s.clone()),
            ExprKind::Paren(x) => self.const_eval(x, prev)?,
            ExprKind::Unary(UnaryOp::Neg, x) => match self.const_eval(x, prev)? {
                EnumVal::Num(n) => EnumVal::Num(-n),
                _ => return None,
            },
            ExprKind::Ident(n) => prev.iter().find(|(m, _)| m == n)?.1.clone(),
            ExprKind::Binary(op, a, b) => {
                let (EnumVal::Num(x), EnumVal::Num(y)) = (self.const_eval(a, prev)?, self.const_eval(b, prev)?) else {
                    return None;
                };
                let (xi, yi) = (crate::runtime::natives::to_int32(x), crate::runtime::natives::to_int32(y));
                EnumVal::Num(match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    BinOp::Mul => x * y,
                    BinOp::Div => x / y,
                    BinOp::Shl => xi.wrapping_shl(yi as u32 & 31) as f64,
                    BinOp::Shr => (xi >> (yi as u32 & 31)) as f64,
                    BinOp::BitOr => (xi | yi) as f64,
                    BinOp::BitAnd => (xi & yi) as f64,
                    BinOp::BitXor => (xi ^ yi) as f64,
                    _ => return None,
                })
            }
            _ => return None,
        })
    }

    fn declare_class(&mut self, name: &Name, decl: Rc<ast::ClassDecl>, scope: &Rc<Scope>) -> CResult<ClassId> {
        let cid = self.classes.len() as ClassId;
        let cscope = Scope::new(Some(scope.clone()), scope.owner);
        let mut tps = Vec::new();
        for tp in &decl.tparams {
            let id = self.new_tparam(&tp.name, None);
            cscope.types.borrow_mut().insert(tp.name.clone(), TB::TParam(id));
            tps.push(id);
        }
        let prelude_error = scope.owner.is_none() && matches!(&**name, "Error" | "TypeError" | "RangeError" | "SyntaxError") && self.classes.len() < 4;
        self.classes.push(ClassDef {
            name: name.clone(),
            decl: decl.clone(),
            scope: scope.clone(),
            cscope,
            tparams: tps,
            parent: None,
            fields: vec![],
            own_field_start: 0,
            methods: Default::default(),
            vtable: vec![],
            ctor: 0,
            statics: Default::default(),
            accessors: Default::default(),
            resolved: false,
            is_abstract: decl.is_abstract,
            prelude_error,
        });
        scope.types.borrow_mut().insert(name.clone(), TB::Class(cid));
        scope.vals.borrow_mut().insert(name.clone(), VB::Class(cid));
        Ok(cid)
    }

    pub fn check_block(&mut self, stmts: &[Stmt]) -> CResult<HStmt> {
        let scope = Scope::new(Some(self.ctx().scope.clone()), Some(self.ctx().func));
        let hs = self.check_block_in(stmts, scope.clone())?;
        let vars = scope.decls.borrow().clone();
        Ok(HStmt::Block(hs, vars))
    }

    /// Check a statement as the body of a control structure (gets its own scope).
    fn check_body(&mut self, s: &Stmt) -> CResult<Vec<HStmt>> {
        match &s.kind {
            StmtKind::Block(b) => Ok(vec![self.check_block(b)?]),
            _ => Ok(vec![self.check_block(std::slice::from_ref(s))?]),
        }
    }

    pub fn check_stmt(&mut self, s: &Stmt, label: Option<Name>) -> CResult<Vec<HStmt>> {
        let span = s.span;
        Ok(match &s.kind {
            StmtKind::Empty
            | StmtKind::Func(_)
            | StmtKind::Interface { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Enum { .. } => vec![],
            StmtKind::Class(c) => {
                let name = c.name.clone().unwrap();
                let Some(VB::Class(cid)) = self.lookup_val(&name) else { unreachable!() };
                self.ensure_class(cid)?;
                let mut out = Vec::new();
                let statics: Vec<(Name, VB)> = self.classes[cid as usize].statics.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                for (_, b) in &statics {
                    if let VB::Func(v, f) = b {
                        out.push(HStmt::MakeClosure(*v, *f));
                    }
                }
                let inits: Vec<(ClassId, VarId, Expr)> = self.static_inits.iter().filter(|(c, _, _)| *c == cid).cloned().collect();
                self.static_inits.retain(|(c, _, _)| *c != cid);
                for (_, v, e) in inits {
                    let ty = self.vars[v as usize].ty.clone();
                    let h = self.check_expr(&e, Some(&ty))?;
                    self.expect_assignable(&h.ty, &ty, e.span)?;
                    out.push(HStmt::Init(v, Some(h.cast(ty))));
                }
                out
            }
            StmtKind::Var { kind, decls, declare } => {
                let mut out = Vec::new();
                for d in decls {
                    if *declare {
                        continue;
                    }
                    match &d.pat {
                        Pattern::Ident(n, sp) => {
                            let Some(VB::Var(v)) = self.lookup_val(n) else {
                                return err(format!("internal: undeclared '{n}'"), *sp);
                            };
                            let declared = if self.var_pending[v as usize] { None } else { Some(self.vars[v as usize].ty.clone()) };
                            match &d.init {
                                Some(init) => {
                                    let h = self.check_expr(init, declared.as_ref())?;
                                    let ty = match &declared {
                                        Some(t) => {
                                            self.expect_assignable(&h.ty, t, init.span)?;
                                            t.clone()
                                        }
                                        None => {
                                            let t = match &h.ty {
                                                Type::Null | Type::Undefined if *kind != VarKind::Const => Type::Any,
                                                Type::Never => Type::Any,
                                                t if *kind == VarKind::Const => t.clone(),
                                                t => t.widen(),
                                            };
                                            self.vars[v as usize].ty = t.clone();
                                            self.var_pending[v as usize] = false;
                                            t
                                        }
                                    };
                                    // Declaration narrowing: `let x: T | null = nonNull`.
                                    if matches!(ty, Type::Union(_)) && !h.ty.is_dynamic() {
                                        let nt = h.ty.widen();
                                        if self.assignable(&nt, &ty) && nt != ty {
                                            self.ctx_mut().narrow.insert(format!("{v}"), nt);
                                        }
                                    } else {
                                        let k = format!("{v}");
                                        self.ctx_mut().narrow.remove(&k);
                                    }
                                    if *kind == VarKind::Const
                                        && matches!(h.kind, HK::Num(_) | HK::Str(_) | HK::Bool(_))
                                        && matches!(
                                            ty,
                                            Type::Number | Type::String | Type::Boolean | Type::NumLit(_) | Type::StrLit(_) | Type::BoolLit(_)
                                        )
                                    {
                                        self.var_const_val[v as usize] = Some(h.clone().cast(ty.clone()));
                                    }
                                    out.push(HStmt::Init(v, Some(h.cast(ty))));
                                }
                                None => {
                                    if *kind == VarKind::Const {
                                        return err("'const' declarations must be initialized", span);
                                    }
                                    if declared.is_none() {
                                        self.var_pending[v as usize] = false;
                                    }
                                    out.push(HStmt::Init(v, None));
                                }
                            }
                        }
                        pat => {
                            let Some(init) = &d.init else {
                                return err("destructuring declarations must be initialized", span);
                            };
                            let want = match &d.ty {
                                Some(t) => Some(self.resolve_type(t)?),
                                None => None,
                            };
                            let h = self.check_expr(init, want.as_ref())?;
                            let ty = want.unwrap_or(h.ty.clone());
                            let scope = self.ctx().scope.clone();
                            let tmp = self.new_var("$d".into(), Some(ty.clone()), true, &scope);
                            out.push(HStmt::Init(tmp, Some(h.cast(ty.clone()))));
                            let src = HExpr::read(Place::Var(tmp), ty);
                            out.extend(self.bind_pattern(pat, src, *kind, Some(span))?);
                        }
                    }
                }
                out
            }
            StmtKind::Expr(e) => {
                let is_super = matches!(&e.kind, ExprKind::Call { callee, .. } if matches!(callee.kind, ExprKind::Super));
                let h = self.check_expr(e, None)?;
                let mut out = vec![HStmt::Expr(h)];
                if is_super {
                    out.extend(self.ctx_mut().ctor_inits.take().unwrap_or_default());
                }
                out
            }
            StmtKind::Block(b) => vec![self.check_block(b)?],
            StmtKind::If(c, t, e) => {
                let hc = self.check_expr(c, None)?;
                let (rt, rf) = (self.refine(c, true)?, self.refine(c, false)?);
                let saved = self.ctx().narrow.clone();
                self.apply(&rt);
                let ht = self.check_body(t)?;
                let after_t = std::mem::replace(&mut self.ctx_mut().narrow, saved.clone());
                self.apply(&rf);
                let he = match e {
                    Some(e) => self.check_body(e)?,
                    None => vec![],
                };
                let after_e = std::mem::replace(&mut self.ctx_mut().narrow, saved);
                // Merge: keep narrowings that hold on both paths.
                let t_exits = stmt_terminates(t);
                let e_exits = e.as_ref().is_some_and(|e| stmt_terminates(e));
                let merged = if t_exits && !e_exits {
                    after_e
                } else if e_exits && !t_exits {
                    after_t
                } else {
                    let mut m = Narrow::default();
                    for (k, a) in &after_t {
                        if let Some(b) = after_e.get(k) {
                            m.insert(k.clone(), Type::union([a.clone(), b.clone()]));
                        }
                    }
                    m
                };
                // Only narrowings that the branches introduced (or kept) survive.
                self.ctx_mut().narrow = merged;
                let _ = (rt, rf);
                vec![HStmt::If(hc, flatten(ht), flatten(he))]
            }
            StmtKind::While(c, body) => {
                self.loop_invalidate(s);
                let hc = self.check_expr(c, None)?;
                let saved = self.ctx().narrow.clone();
                let rt = self.refine(c, true)?;
                self.apply(&rt);
                let hb = self.check_body(body)?;
                self.ctx_mut().narrow = saved;
                self.loop_invalidate(s);
                if !contains_break(body) {
                    let rf = self.refine(c, false)?;
                    self.apply(&rf);
                }
                vec![HStmt::Loop {
                    label,
                    test: Some(hc),
                    body: flatten(hb),
                    update: None,
                    test_after: false,
                    fresh: vec![],
                }]
            }
            StmtKind::DoWhile(body, c) => {
                self.loop_invalidate(s);
                let hb = self.check_body(body)?;
                let hc = self.check_expr(c, None)?;
                self.loop_invalidate(s);
                vec![HStmt::Loop {
                    label,
                    test: Some(hc),
                    body: flatten(hb),
                    update: None,
                    test_after: true,
                    fresh: vec![],
                }]
            }
            StmtKind::For { init, test, update, body } => {
                let scope = self.push_scope();
                let r = (|| -> CResult<Vec<HStmt>> {
                    let mut pre = Vec::new();
                    let mut fresh = Vec::new();
                    if let Some(i) = init {
                        let hs = self.check_block_in_nohoist(i)?;
                        if let StmtKind::Var { kind: VarKind::Let, .. } = &i.kind {
                            fresh = scope.decls.borrow().clone();
                        }
                        pre.extend(hs);
                    }
                    self.loop_invalidate(s);
                    let ht = match test {
                        Some(t) => Some(self.check_expr(t, None)?),
                        None => None,
                    };
                    let saved = self.ctx().narrow.clone();
                    if let Some(t) = test {
                        let rt = self.refine(t, true)?;
                        self.apply(&rt);
                    }
                    let hb = self.check_body(body)?;
                    let hu = match update {
                        Some(u) => Some(self.check_expr(u, None)?),
                        None => None,
                    };
                    self.ctx_mut().narrow = saved;
                    self.loop_invalidate(s);
                    pre.push(HStmt::Loop {
                        label: label.clone(),
                        test: ht,
                        body: flatten(hb),
                        update: hu,
                        test_after: false,
                        fresh,
                    });
                    Ok(pre)
                })();
                self.pop_scope();
                let vars = scope.decls.borrow().clone();
                vec![HStmt::Block(r?, vars)]
            }
            StmtKind::ForOf { kind, pat, iter, body } => {
                let hi = self.check_expr(iter, None)?;
                let Some(et) = self.iter_elem(&hi.ty) else {
                    return err(format!("type '{}' is not iterable", self.show(&hi.ty)), iter.span);
                };
                self.loop_invalidate(s);
                let scope = self.push_scope();
                let r = (|| -> CResult<HStmt> {
                    let (elem, pre) = match pat {
                        Pattern::Ident(n, _) => {
                            let v = self.new_var(n.clone(), Some(et.clone()), *kind == VarKind::Const, &scope);
                            scope.decls.borrow_mut().pop();
                            scope.vals.borrow_mut().insert(n.clone(), VB::Var(v));
                            (v, vec![])
                        }
                        pat => {
                            let v = self.new_var("$elem".into(), Some(et.clone()), true, &scope);
                            scope.decls.borrow_mut().pop();
                            let src = HExpr::read(Place::Var(v), et.clone());
                            let pre = self.bind_pattern(pat, src, *kind, Some(span))?;
                            (v, pre)
                        }
                    };
                    let saved = self.ctx().narrow.clone();
                    let hb = self.check_body(body)?;
                    self.ctx_mut().narrow = saved;
                    let vars = scope.decls.borrow().clone();
                    let mut b = pre;
                    b.extend(flatten(hb));
                    Ok(HStmt::ForOf {
                        label: label.clone(),
                        elem,
                        iter: hi,
                        body: vec![HStmt::Block(b, vars)],
                    })
                })();
                self.pop_scope();
                self.loop_invalidate(s);
                vec![r?]
            }
            StmtKind::ForIn { kind, name, obj, body } => {
                let ho = self.check_expr(obj, None)?;
                self.loop_invalidate(s);
                let scope = self.push_scope();
                let r = (|| -> CResult<HStmt> {
                    let v = self.new_var(name.clone(), Some(Type::String), *kind == VarKind::Const, &scope);
                    scope.decls.borrow_mut().pop();
                    scope.vals.borrow_mut().insert(name.clone(), VB::Var(v));
                    let hb = self.check_body(body)?;
                    Ok(HStmt::ForIn {
                        label: label.clone(),
                        key: v,
                        obj: ho,
                        body: flatten(hb),
                    })
                })();
                self.pop_scope();
                self.loop_invalidate(s);
                vec![r?]
            }
            StmtKind::Break(l) => vec![HStmt::Break(l.clone())],
            StmtKind::Continue(l) => vec![HStmt::Continue(l.clone())],
            StmtKind::Return(v) => {
                if self.ctx().func == MAIN {
                    return err("'return' outside of a function", span);
                }
                let want = self.ctx().ret.clone();
                match v {
                    Some(e) => {
                        let h = self.check_expr(e, want.as_ref())?;
                        if let Some(w) = &want {
                            if !matches!(w, Type::Void) || !matches!(h.ty, Type::Undefined | Type::Void) {
                                if matches!(w, Type::Void) {
                                    return err("a 'void' function cannot return a value", e.span);
                                }
                                self.expect_assignable(&h.ty, w, e.span)?;
                            }
                        }
                        self.ctx_mut().returns.push(h.ty.clone());
                        let h = match &want {
                            Some(w) if !matches!(w, Type::Void) => h.cast(w.clone()),
                            _ => h,
                        };
                        vec![HStmt::Return(Some(h))]
                    }
                    None => {
                        if let Some(w) = &want
                            && !matches!(w, Type::Void | Type::Any | Type::Undefined)
                            && !self.assignable(&Type::Undefined, w)
                        {
                            return err(format!("a function whose declared type is '{}' must return a value", self.show(w)), span);
                        }
                        if want.is_none() {
                            self.ctx_mut().returns.push(Type::Undefined);
                        }
                        vec![HStmt::Return(None)]
                    }
                }
            }
            StmtKind::Throw(e) => vec![HStmt::Throw(self.check_expr(e, None)?)],
            StmtKind::Try {
                block,
                param,
                handler,
                finalizer,
            } => {
                let saved = self.ctx().narrow.clone();
                let hb = self.check_block(block)?;
                self.ctx_mut().narrow = saved.clone();
                let mut catch_var = None;
                let mut hh = Vec::new();
                if let Some(h) = handler {
                    let scope = self.push_scope();
                    let r = (|| -> CResult<Vec<HStmt>> {
                        let mut pre = Vec::new();
                        match param {
                            Some(Pattern::Ident(n, _)) => {
                                let v = self.new_var(n.clone(), Some(Type::Any), false, &scope);
                                scope.decls.borrow_mut().pop();
                                scope.vals.borrow_mut().insert(n.clone(), VB::Var(v));
                                catch_var = Some(v);
                            }
                            Some(p) => {
                                let v = self.new_var("$exc".into(), Some(Type::Any), false, &scope);
                                scope.decls.borrow_mut().pop();
                                catch_var = Some(v);
                                pre = self.bind_pattern(p, HExpr::read(Place::Var(v), Type::Any), VarKind::Let, Some(span))?;
                            }
                            None => {}
                        }
                        let hs = self.check_block_in(h, scope.clone())?;
                        pre.extend(hs);
                        let vars = scope.decls.borrow().clone();
                        Ok(vec![HStmt::Block(pre, vars)])
                    })();
                    self.pop_scope();
                    hh = r?;
                }
                self.ctx_mut().narrow = saved.clone();
                let hf = match finalizer {
                    Some(f) => Some(vec![self.check_block(f)?]),
                    None => None,
                };
                self.ctx_mut().narrow = saved;
                vec![HStmt::Try {
                    block: vec![hb],
                    catch_var,
                    handler: hh,
                    finalizer: hf,
                }]
            }
            StmtKind::Switch(d, cases) => {
                let hd = self.check_expr(d, None)?;
                let scope = self.push_scope();
                let r = (|| -> CResult<HStmt> {
                    // Hoist declarations across all cases.
                    let all: Vec<Stmt> = cases.iter().flat_map(|c| c.body.iter().cloned()).collect();
                    let _ = all;
                    let mut hcases = Vec::new();
                    let saved = self.ctx().narrow.clone();
                    let mut pending_labels: Vec<&Expr> = Vec::new();
                    for c in cases.iter() {
                        let ht = match &c.test {
                            Some(t) => {
                                let h = self.check_expr(t, Some(&hd.ty))?;
                                Some(h)
                            }
                            None => None,
                        };
                        // Narrow the discriminant for this case's body.
                        self.ctx_mut().narrow = saved.clone();
                        if let Some(t) = &c.test {
                            pending_labels.push(t);
                        }
                        if !c.body.is_empty() {
                            if c.test.is_some() && !pending_labels.is_empty() {
                                let refs = self.refine_switch(d, &pending_labels)?;
                                self.apply(&refs);
                            }
                            pending_labels.clear();
                        }
                        let hb = self.check_block_in(&c.body, scope.clone())?;
                        hcases.push((ht, hb));
                    }
                    self.ctx_mut().narrow = saved;
                    Ok(HStmt::Switch {
                        label: label.clone(),
                        disc: hd,
                        cases: hcases,
                    })
                })();
                self.pop_scope();
                let vars = scope.decls.borrow().clone();
                vec![HStmt::Block(vec![r?], vars)]
            }
            StmtKind::Labeled(name, body) => match &body.kind {
                StmtKind::While(..) | StmtKind::DoWhile(..) | StmtKind::For { .. } | StmtKind::ForOf { .. } | StmtKind::ForIn { .. } => {
                    self.check_stmt(body, Some(name.clone()))?
                }
                _ => {
                    let inner = self.check_body(body)?;
                    vec![HStmt::Labeled(name.clone(), Box::new(HStmt::Block(flatten(inner), vec![])))]
                }
            },
        })
    }

    /// For-loop initializers are declared in the loop scope (already pushed).
    fn check_block_in_nohoist(&mut self, s: &Stmt) -> CResult<Vec<HStmt>> {
        if let StmtKind::Var { kind, decls, .. } = &s.kind {
            let scope = self.ctx().scope.clone();
            for d in decls {
                let mut names = Vec::new();
                pattern_names(&d.pat, &mut names);
                for (n, _) in names {
                    let ty = match (&d.pat, &d.ty) {
                        (Pattern::Ident(..), Some(t)) => Some(self.resolve_type(t)?),
                        _ => None,
                    };
                    let target = if *kind == VarKind::Var { self.ctx().fn_scope.clone() } else { scope.clone() };
                    let v = self.new_var(n.clone(), ty, *kind == VarKind::Const, &target);
                    target.vals.borrow_mut().insert(n, VB::Var(v));
                }
            }
        }
        self.check_stmt(s, None)
    }

    fn loop_invalidate(&mut self, s: &Stmt) {
        let mut names = FxHashSet::default();
        assigned_names(s, &mut names);
        if names.is_empty() {
            return;
        }
        let vars = &self.vars;
        let this_v = self.stack.last().and_then(|c| c.this_var);
        self.stack.last_mut().unwrap().narrow.retain(|k, _| {
            let root: usize = k.split('.').next().unwrap().parse().unwrap_or(usize::MAX);
            if root >= vars.len() {
                return true;
            }
            let n = if Some(root as u32) == this_v { "this".into() } else { vars[root].name.clone() };
            !names.contains(&n)
        });
    }

    // ───────────── destructuring ─────────────

    pub fn bind_pattern(&mut self, pat: &Pattern, src: HExpr, kind: VarKind, span: Option<Span>) -> CResult<Vec<HStmt>> {
        let span = span.unwrap_or(pat.span());
        let mut out = Vec::new();
        match pat {
            Pattern::Ident(n, sp) => {
                let v = match self.lookup_val(n) {
                    Some(VB::Var(v)) if self.var_pending[v as usize] || self.ctx().scope.vals.borrow().contains_key(n) => v,
                    _ => {
                        let scope = self.ctx().scope.clone();
                        let v = self.new_var(n.clone(), None, kind == VarKind::Const, &scope);
                        scope.vals.borrow_mut().insert(n.clone(), VB::Var(v));
                        v
                    }
                };
                let ty = if self.var_pending[v as usize] {
                    let t = src.ty.widen();
                    self.vars[v as usize].ty = t.clone();
                    self.var_pending[v as usize] = false;
                    t
                } else {
                    let t = self.vars[v as usize].ty.clone();
                    self.expect_assignable(&src.ty, &t, *sp)?;
                    t
                };
                out.push(HStmt::Init(v, Some(src.cast(ty))));
            }
            Pattern::Object { props, rest, .. } => {
                if rest.is_some() {
                    return err("object rest patterns are not supported yet", span);
                }
                let tmp = self.stash(src, &mut out);
                for (key, p, default) in props {
                    let val = self.member_read(tmp.clone(), key, false, span)?;
                    let val = self.with_default(val, default.as_ref())?;
                    out.extend(self.bind_pattern(p, val, kind, Some(span))?);
                }
            }
            Pattern::Array { elems, rest, .. } => {
                let tmp = self.stash(src, &mut out);
                for (i, el) in elems.iter().enumerate() {
                    let Some((p, default)) = el else { continue };
                    let val = self.index_read(tmp.clone(), HExpr::num(i as f64), span)?;
                    let val = self.with_default(val, default.as_ref())?;
                    out.extend(self.bind_pattern(p, val, kind, Some(span))?);
                }
                if let Some(r) = rest {
                    let ty = match &tmp.ty {
                        Type::Array(_) => tmp.ty.clone(),
                        Type::Tuple(ts) => Type::array(Type::union(ts.iter().skip(elems.len()).cloned())),
                        _ => Type::array(Type::Any),
                    };
                    let slice = self.native_call(
                        crate::runtime::Native::ArrSlice,
                        Some(tmp.clone()),
                        vec![HElem::Expr(HExpr::num(elems.len() as f64))],
                        ty,
                    );
                    out.extend(self.bind_pattern(r, slice, kind, Some(span))?);
                }
            }
        }
        Ok(out)
    }

    /// Evaluate `src` once into a hidden variable (unless it already is a variable read).
    fn stash(&mut self, src: HExpr, out: &mut Vec<HStmt>) -> HExpr {
        if let HK::Read(p) = &src.kind
            && let Place::Var(_) = **p
        {
            return src;
        }
        let scope = self.ctx().scope.clone();
        let ty = src.ty.clone();
        let v = self.new_var("$p".into(), Some(ty.clone()), true, &scope);
        out.push(HStmt::Init(v, Some(src)));
        HExpr::read(Place::Var(v), ty)
    }

    fn with_default(&mut self, val: HExpr, default: Option<&Expr>) -> CResult<HExpr> {
        let Some(d) = default else { return Ok(val) };
        let base = val.ty.non_null();
        let hd = self.check_expr(d, Some(&base))?;
        let ty = Type::union([base, hd.ty.clone()]);
        // val === undefined ? d : val   (val is a plain read; evaluating twice is fine)
        let test = HExpr::new(HK::Binary(HBin::StrictEq(true), Box::new(val.clone()), Box::new(HExpr::undefined())), Type::Boolean);
        Ok(HExpr::new(HK::Cond(Box::new(test), Box::new(hd), Box::new(val)), ty))
    }

    // ───────────── narrowing ─────────────

    pub fn apply(&mut self, refs: &[(String, Type)]) {
        for (k, t) in refs {
            self.ctx_mut().narrow.insert(k.clone(), t.clone());
        }
    }

    /// Current type of a narrowable expression path.
    fn current_type(&mut self, e: &Expr) -> CResult<Option<(String, Type)>> {
        let Some(k) = self.narrow_key(e) else { return Ok(None) };
        let h = self.check_expr(e, None)?;
        Ok(Some((k, h.ty)))
    }

    /// Refinements that hold when `e` evaluates to `truthy`.
    pub fn refine(&mut self, e: &Expr, truthy: bool) -> CResult<Vec<(String, Type)>> {
        let mut out = Vec::new();
        match &e.kind {
            ExprKind::Paren(x) => return self.refine(x, truthy),
            ExprKind::Unary(UnaryOp::Not, x) => return self.refine(x, !truthy),
            ExprKind::Logical(crate::ast::LogicalOp::And, a, b) if truthy => {
                let ra = self.refine(a, true)?;
                let saved = self.ctx().narrow.clone();
                self.apply(&ra);
                let rb = self.refine(b, true)?;
                self.ctx_mut().narrow = saved;
                out.extend(ra);
                out.extend(rb);
            }
            ExprKind::Logical(crate::ast::LogicalOp::Or, a, b) if !truthy => {
                let ra = self.refine(a, false)?;
                let saved = self.ctx().narrow.clone();
                self.apply(&ra);
                let rb = self.refine(b, false)?;
                self.ctx_mut().narrow = saved;
                out.extend(ra);
                out.extend(rb);
            }
            ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::This => {
                if let Some((k, t)) = self.current_type(e)? {
                    let nt = if truthy { expr::truthy_part(&t) } else { expr::falsy_part(&t) };
                    if !matches!(nt, Type::Never) {
                        out.push((k, nt));
                    }
                }
            }
            ExprKind::Binary(op @ (BinOp::StrictEq | BinOp::StrictNe | BinOp::Eq | BinOp::Ne), a, b) => {
                let positive = matches!(op, BinOp::StrictEq | BinOp::Eq) == truthy;
                let loose = matches!(op, BinOp::Eq | BinOp::Ne);
                // typeof x === "tag"
                for (l, r) in [(a, b), (b, a)] {
                    if let (ExprKind::Unary(UnaryOp::TypeOf, x), ExprKind::Str(tag)) = (&l.kind, &r.kind)
                        && let Some((k, t)) = self.current_type(x)?
                    {
                        out.push((k, filter_typeof(&t, tag, positive)));
                        return Ok(out);
                    }
                }
                for (l, r) in [(a, b), (b, a)] {
                    let lit = literal_of(r);
                    let Some(lit) = lit else { continue };
                    // x === null / undefined / literal
                    if let Some((k, t)) = self.current_type(l)? {
                        let nt = narrow_eq(&t, &lit, positive, loose);
                        if !matches!(nt, Type::Never) || positive {
                            out.push((k, nt));
                        }
                        // Discriminant: x.kind === "lit" also narrows x.
                    }
                    if let ExprKind::Member { obj, prop, optional: false } = &l.kind
                        && let Some((ok, ot)) = self.current_type(obj)?
                        && matches!(ot, Type::Union(_))
                    {
                        let filtered = self.filter_discriminant(&ot, prop, &lit, positive);
                        out.push((ok, filtered));
                    }
                    return Ok(out);
                }
            }
            ExprKind::Binary(BinOp::InstanceOf, a, b) => {
                if let ExprKind::Ident(n) = &b.kind
                    && let Some(VB::Class(cid)) = self.lookup_val(n)
                    && let Some((k, t)) = self.current_type(a)?
                {
                    let nt = self.filter_instance(&t, cid, truthy);
                    out.push((k, nt));
                }
            }
            ExprKind::Binary(BinOp::In, a, b) => {
                if let ExprKind::Str(prop) = &a.kind
                    && let Some((k, t)) = self.current_type(b)?
                    && matches!(t, Type::Union(_))
                {
                    let ms: Vec<Type> = t
                        .members()
                        .into_iter()
                        .filter(|m| {
                            let has = self.obj_view(m).is_some_and(|o| o.prop(prop).is_some());
                            has == truthy
                        })
                        .collect();
                    out.push((k, Type::union(ms)));
                }
            }
            ExprKind::Call { callee, args, .. } => {
                // Array.isArray(x)
                if let ExprKind::Member { obj, prop, .. } = &callee.kind
                    && matches!(&obj.kind, ExprKind::Ident(n) if &**n == "Array")
                    && &**prop == "isArray"
                    && let Some(crate::ast::ArrayElem::Expr(x)) = args.first()
                    && let Some((k, t)) = self.current_type(x)?
                {
                    let ms: Vec<Type> = t
                        .members()
                        .into_iter()
                        .filter(|m| matches!(m, Type::Array(_) | Type::Tuple(_)) == truthy || m.is_dynamic())
                        .map(|m| if m.is_dynamic() && truthy { Type::array(Type::Any) } else { m })
                        .collect();
                    out.push((k, Type::union(ms)));
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// Narrowing for a switch case body (labels that fall into it).
    fn refine_switch(&mut self, d: &Expr, labels: &[&Expr]) -> CResult<Vec<(String, Type)>> {
        let mut out = Vec::new();
        let lits: Vec<Type> = labels.iter().filter_map(|l| literal_of(l)).collect();
        if lits.len() != labels.len() {
            return Ok(out);
        }
        // switch (typeof x)
        if let ExprKind::Unary(UnaryOp::TypeOf, x) = &d.kind {
            if let Some((k, t)) = self.current_type(x)? {
                let ms: Vec<Type> = lits
                    .iter()
                    .filter_map(|l| match l {
                        Type::StrLit(tag) => Some(filter_typeof(&t, tag, true)),
                        _ => None,
                    })
                    .collect();
                out.push((k, Type::union(ms)));
            }
            return Ok(out);
        }
        if let Some((k, t)) = self.current_type(d)? {
            let ms: Vec<Type> = lits.iter().map(|l| narrow_eq(&t, l, true, false)).collect();
            out.push((k, Type::union(ms)));
        }
        if let ExprKind::Member { obj, prop, optional: false } = &d.kind
            && let Some((ok, ot)) = self.current_type(obj)?
            && matches!(ot, Type::Union(_))
        {
            let ms: Vec<Type> = lits.iter().map(|l| self.filter_discriminant(&ot, prop, l, true)).collect();
            out.push((ok, Type::union(ms)));
        }
        Ok(out)
    }

    fn filter_discriminant(&mut self, t: &Type, prop: &str, lit: &Type, positive: bool) -> Type {
        let ms: Vec<Type> = t
            .members()
            .into_iter()
            .filter(|m| {
                if m.is_nullish() {
                    return !positive;
                }
                let Some(o) = self.obj_view(m) else { return true };
                let Some(p) = o.prop(prop) else { return !positive };
                let pt = p.ty.clone();
                if positive {
                    self.assignable(lit, &pt) || self.assignable(&pt, lit)
                } else {
                    // Exclude members whose property is exactly this literal.
                    pt != *lit
                }
            })
            .collect();
        Type::union(ms)
    }

    fn filter_instance(&mut self, t: &Type, cid: ClassId, keep: bool) -> Type {
        let target = Type::Class(cid, vec![Type::Any; self.classes[cid as usize].tparams.len()].into());
        if t.is_dynamic() {
            return if keep { target } else { t.clone() };
        }
        let ms: Vec<Type> = t
            .members()
            .into_iter()
            .filter(|m| {
                let is = matches!(m, Type::Class(..)) && self.assignable(m, &target);
                if keep { is || !matches!(m, Type::Class(..)) && !m.is_nullish() && !matches!(m, Type::Number | Type::String | Type::Boolean) } else { !is }
            })
            .collect();
        let r = Type::union(ms);
        if keep && matches!(r, Type::Never) { target } else if keep && !matches!(r, Type::Class(..)) && r.members().iter().all(|m| !matches!(m, Type::Class(..))) { target } else { r }
    }
}

fn flatten(v: Vec<HStmt>) -> Vec<HStmt> {
    v
}

/// The literal type of a narrowing comparand, if it is one.
fn literal_of(e: &Expr) -> Option<Type> {
    Some(match &e.kind {
        ExprKind::Null => Type::Null,
        ExprKind::Ident(n) if &**n == "undefined" => Type::Undefined,
        ExprKind::Str(s) => Type::StrLit(s.clone()),
        ExprKind::Bool(b) => Type::BoolLit(*b),
        ExprKind::Num(n) => Type::NumLit(*n),
        ExprKind::Paren(x) => literal_of(x)?,
        _ => return None,
    })
}

fn narrow_eq(t: &Type, lit: &Type, positive: bool, loose: bool) -> Type {
    let nullish_lit = matches!(lit, Type::Null | Type::Undefined);
    if t.is_dynamic() {
        return if positive && nullish_lit && !loose { lit.clone() } else { t.clone() };
    }
    let ms = t.members();
    let matches = |m: &Type| -> bool {
        if loose && nullish_lit {
            return m.is_nullish();
        }
        match (m, lit) {
            (Type::Void, Type::Undefined) => true,
            (a, b) if a == b => true,
            (Type::String, Type::StrLit(_)) | (Type::Number, Type::NumLit(_)) | (Type::Boolean, Type::BoolLit(_)) => positive,
            (Type::Enum(_), Type::NumLit(_)) => positive,
            _ => false,
        }
    };
    if positive {
        let kept: Vec<Type> = ms
            .iter()
            .filter(|m| matches(m))
            .map(|m| match (m, lit) {
                (Type::String, Type::StrLit(_)) | (Type::Boolean, Type::BoolLit(_)) => lit.clone(),
                (m, _) => m.clone(),
            })
            .collect();
        Type::union(kept)
    } else {
        // Only exact singleton members can be removed.
        let kept: Vec<Type> = ms
            .iter()
            .filter(|m| {
                if loose && nullish_lit {
                    return !m.is_nullish();
                }
                !(**m == *lit || (matches!(m, Type::Void) && *lit == Type::Undefined))
            })
            .cloned()
            .collect();
        Type::union(kept)
    }
}

fn typeof_tag(t: &Type) -> &'static str {
    match t {
        Type::Number | Type::NumLit(_) | Type::Enum(_) => "number",
        Type::BigInt => "bigint",
        Type::String | Type::StrLit(_) => "string",
        Type::Boolean | Type::BoolLit(_) => "boolean",
        Type::Undefined | Type::Void => "undefined",
        Type::Func(_) | Type::ClassCtor(_) => "function",
        _ => "object",
    }
}

fn filter_typeof(t: &Type, tag: &str, keep: bool) -> Type {
    if t.is_dynamic() {
        if !keep {
            return t.clone();
        }
        return match tag {
            "number" => Type::Number,
            "bigint" => Type::BigInt,
            "string" => Type::String,
            "boolean" => Type::Boolean,
            "undefined" => Type::Undefined,
            "function" => Type::Func(Rc::new(FuncType {
                tparams: vec![],
                params: vec![FParam {
                    name: "args".into(),
                    ty: Type::array(Type::Any),
                    optional: false,
                    rest: true,
                }],
                ret: Type::Any,
            })),
            _ => t.clone(),
        };
    }
    let ms: Vec<Type> = t
        .members()
        .into_iter()
        .filter(|m| {
            if matches!(m, Type::Param(_)) {
                return true;
            }
            (typeof_tag(m) == tag) == keep
        })
        .collect();
    Type::union(ms)
}
