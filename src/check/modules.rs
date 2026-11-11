//! Modules in the checker. Each module is a scope under the global scope
//! (built-ins and the prelude). Its declarations are hoisted first in every
//! module, then imports are bound by aliasing the exporter's bindings (the same
//! `VarId`, so an import is a live binding), then each module's statements are
//! checked in evaluation order as part of the one top-level function.

use std::rc::Rc;

use crate::ast::*;
use crate::error::Span;
use crate::hir::*;
use crate::module::{ExportTarget, Exports, ModuleGraph, import_of, ImportOf};

use super::*;

/// One module, rewritten for checking.
struct Prepared {
    stmts: Vec<Stmt>,
    /// Local name → what it is bound to (imports and `ns.x` aliases).
    bindings: Vec<(Name, ExportTarget, Span, bool)>,
}

impl Checker {
    pub fn check_modules(&mut self, g: &ModuleGraph) -> CResult<Vec<HStmt>> {
        let exports = Exports::new(g);
        let n = g.modules.len();
        let mut prepared = Vec::with_capacity(n);
        for m in 0..n {
            prepared.push(prepare(g, &exports, m)?);
        }
        let scopes: Vec<Rc<Scope>> = (0..n).map(|_| Scope::new(Some(self.module.clone()), None)).collect();
        self.module_scopes = scopes.clone();

        let mut classes = vec![Vec::new(); n];
        for &m in &g.order {
            self.enter_module(&scopes[m]);
            classes[m] = self.hoist_types(&prepared[m].stmts, &scopes[m])?;
        }
        for &m in &g.order {
            for (local, target, _, _) in &prepared[m].bindings {
                if let ExportTarget::Local(dm, dn) = target
                    && let Some(tb) = scopes[*dm].types.borrow().get(dn).cloned()
                {
                    scopes[m].types.borrow_mut().insert(local.clone(), tb);
                }
            }
        }
        let mut hoisted = Vec::new();
        let mut funcs = vec![Vec::new(); n];
        for &m in &g.order {
            self.enter_module(&scopes[m]);
            funcs[m] = self.hoist_values(&prepared[m].stmts, &scopes[m], &mut hoisted)?;
        }
        for &m in &g.order {
            for (local, target, span, type_only) in &prepared[m].bindings {
                let vb = match target {
                    ExportTarget::Local(dm, dn) => scopes[*dm].vals.borrow().get(dn).cloned(),
                    ExportTarget::Namespace(dm) => Some(VB::Namespace(*dm as u32)),
                };
                match vb {
                    Some(vb) => {
                        scopes[m].vals.borrow_mut().insert(local.clone(), vb);
                    }
                    None => {
                        let ExportTarget::Local(dm, dn) = target else { unreachable!() };
                        let is_type = scopes[*dm].types.borrow().contains_key(dn);
                        if !is_type && !type_only && !local.contains('.') {
                            return err(format!("module '{}' has no export named '{dn}'", short(&g.modules[*dm].path)), *span);
                        }
                    }
                }
            }
        }
        // What each module's namespace object holds.
        for m in 0..n {
            let mut members = Vec::new();
            for name in exports.names(m) {
                let vb = match exports.resolve(g, m, &name) {
                    Some(ExportTarget::Local(dm, dn)) => scopes[dm].vals.borrow().get(&dn).cloned(),
                    Some(ExportTarget::Namespace(dm)) => Some(VB::Namespace(dm as u32)),
                    None => None,
                };
                if let Some(vb) = vb {
                    members.push((name, vb));
                }
            }
            self.namespaces.push(members);
        }
        let mut body = hoisted;
        for &m in &g.order {
            self.enter_module(&scopes[m]);
            let stmts = std::mem::take(&mut prepared[m].stmts);
            let hs = self.check_hoisted(&stmts, Vec::new(), std::mem::take(&mut funcs[m]), std::mem::take(&mut classes[m]))?;
            body.extend(hs);
        }
        let global = self.module.clone();
        self.enter_module(&global);
        Ok(body)
    }

    fn enter_module(&mut self, scope: &Rc<Scope>) {
        let ctx = self.ctx_mut();
        ctx.scope = scope.clone();
        ctx.fn_scope = scope.clone();
        ctx.narrow.clear();
    }

    /// A module namespace used as a value: an object of its exports.
    pub fn namespace_object(&mut self, m: u32, span: Span) -> CResult<HExpr> {
        let members = self.namespaces[m as usize].clone();
        let mut props = Vec::new();
        for (name, vb) in members {
            let hidden: Name = format!("\u{1}ns{m}.{name}").into();
            self.module.vals.borrow_mut().insert(hidden.clone(), vb);
            props.push(ObjProp::Init {
                key: PropKey::Name(name),
                value: Expr {
                    kind: ExprKind::Ident(hidden),
                    span,
                },
            });
        }
        self.check_expr(
            &Expr {
                kind: ExprKind::Object(props),
                span,
            },
            None,
        )
    }
}

fn short(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    std::path::Path::new(path).strip_prefix(&cwd).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

/// Rewrite module `m` for checking: drop imports (recording what they bind),
/// unwrap exported declarations, and turn `ns.x` for a namespace import `ns`
/// into a reference to the binding itself.
fn prepare(g: &ModuleGraph, exports: &Exports, m: usize) -> CResult<Prepared> {
    let module = &g.modules[m];
    let mut bindings = Vec::new();
    let mut namespaces: Vec<(Name, usize)> = Vec::new();
    let mut stmts = Vec::new();
    for s in &module.stmts {
        match &s.kind {
            StmtKind::Import(d) => {
                let dep = module.deps[&d.from];
                if let Some(local) = &d.default {
                    let t = exports.resolve(g, dep, "default");
                    match t {
                        Some(t) => bindings.push((local.clone(), t, d.span, d.type_only)),
                        None => return err(format!("module '{}' has no default export", d.from), d.span),
                    }
                }
                if let Some(ns) = &d.namespace {
                    bindings.push((ns.clone(), ExportTarget::Namespace(dep), d.span, d.type_only));
                    namespaces.push((ns.clone(), dep));
                }
                for spec in &d.named {
                    match exports.resolve(g, dep, &spec.imported) {
                        Some(t) => {
                            if let ExportTarget::Namespace(nm) = t {
                                namespaces.push((spec.local.clone(), nm));
                            }
                            bindings.push((spec.local.clone(), t, d.span, d.type_only || spec.type_only));
                        }
                        None if d.type_only || spec.type_only => {}
                        None => {
                            return err(
                                format!("module '{}' has no export named '{}'", d.from, spec.imported),
                                d.span,
                            );
                        }
                    }
                }
            }
            StmtKind::Export(e) => match &**e {
                ExportDecl::Decl(d) => stmts.push(d.clone()),
                ExportDecl::DefaultDecl(d) => {
                    let mut d = d.clone();
                    match &mut d.kind {
                        StmtKind::Func(f) if f.name.is_none() => Rc::make_mut(f).name = Some("default".into()),
                        StmtKind::Class(c) if c.name.is_none() => Rc::make_mut(c).name = Some("default".into()),
                        _ => {}
                    }
                    stmts.push(d);
                }
                ExportDecl::DefaultExpr(e) => stmts.push(Stmt {
                    kind: StmtKind::Var {
                        kind: VarKind::Const,
                        decls: vec![VarDeclarator {
                            pat: Pattern::Ident("default".into(), e.span),
                            ty: None,
                            init: Some(e.clone()),
                        }],
                        declare: false,
                    },
                    span: s.span,
                }),
                ExportDecl::Named { specs, from: None, span } => {
                    // `export { x }` of something neither declared nor imported here.
                    for (local, _) in specs {
                        let declared = module.stmts.iter().any(|s| declares(s, local));
                        if !declared && import_of(g, m, local).is_none() {
                            return err(format!("cannot export '{local}': it is not declared in this module"), *span);
                        }
                    }
                }
                ExportDecl::Named { .. } | ExportDecl::All { .. } => {}
            },
            _ => stmts.push(s.clone()),
        }
    }
    if !namespaces.is_empty() {
        // `ns.x` → a hidden name bound to what `x` resolves to.
        for (ns, dep) in &namespaces {
            for name in exports.names(*dep) {
                if let Some(t) = exports.resolve(g, *dep, &name) {
                    bindings.push((format!("{ns}.{name}").into(), t, Span::default(), true));
                }
            }
        }
        let names: Vec<Name> = namespaces.iter().map(|(n, _)| n.clone()).collect();
        visit_exprs_mut(&mut stmts, &mut |e| {
            if let ExprKind::Member { obj, prop, optional: false } = &e.kind
                && let ExprKind::Ident(n) = &obj.kind
                && names.contains(n)
            {
                e.kind = ExprKind::Ident(format!("{n}.{prop}").into());
            }
        });
    }
    let _ = ImportOf::Namespace;
    Ok(Prepared { stmts, bindings })
}

fn declares(s: &Stmt, name: &str) -> bool {
    let inner = match &s.kind {
        StmtKind::Export(e) => match &**e {
            ExportDecl::Decl(d) | ExportDecl::DefaultDecl(d) => d,
            _ => return false,
        },
        _ => s,
    };
    crate::module::declared_names(inner).iter().any(|n| &**n == name)
}
