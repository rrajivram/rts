//! Modules in the checker. Each module is a scope under the global scope
//! (built-ins and the prelude). Its declarations are hoisted first in every
//! module, then imports are bound by aliasing the exporter's bindings (the same
//! `VarId`, so an import is a live binding), then each module's statements are
//! checked in evaluation order as part of the one top-level function.

use std::rc::Rc;

use crate::ast::*;
use crate::error::Span;
use crate::hir::*;
use crate::module::{CJS_EXPORTS, ExportTarget, Exports, ModuleGraph, import_of, ImportOf};

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
        let mut scopes: Vec<Rc<Scope>> = Vec::with_capacity(n);
        for m in &g.modules {
            // A namespace block sees the names of the module it is written in.
            let parent = m.parent.map_or_else(|| self.module.clone(), |p| scopes[p].clone());
            scopes.push(Scope::new(Some(parent), None));
        }
        self.module_scopes = scopes.clone();

        let typed = !self.erase;
        let mut classes = vec![Vec::new(); n];
        for &m in &g.order {
            self.enter_module(&scopes[m]);
            self.erase = !typed || g.modules[m].is_js;
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
            self.erase = !typed || g.modules[m].is_js;
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
            self.erase = !typed || g.modules[m].is_js;
            let stmts = std::mem::take(&mut prepared[m].stmts);
            let hs = self.check_hoisted(&stmts, Vec::new(), std::mem::take(&mut funcs[m]), std::mem::take(&mut classes[m]))?;
            body.extend(hs);
        }
        let global = self.module.clone();
        self.enter_module(&global);
        self.erase = !typed;
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
    // (local name, alias of the CommonJS module's exports, property)
    let mut cjs_named: Vec<(Name, Name, Name)> = Vec::new();
    // Locals bound to `require()` of a CommonJS module with static exports.
    let mut cjs_ns: Vec<Name> = Vec::new();
    for s in &module.stmts {
        match &s.kind {
            StmtKind::Import(d) if g.modules[module.deps[&d.from]].is_cjs => {
                // Importing CommonJS: the default (and a namespace) is
                // `module.exports`; named imports are its properties.
                let dep = module.deps[&d.from];
                let exports = ExportTarget::Local(dep, CJS_EXPORTS.into());
                for local in d.default.iter().chain(d.namespace.iter()) {
                    bindings.push((local.clone(), exports.clone(), d.span, false));
                }
                if !d.named.is_empty() {
                    let alias: Name = format!("\u{1}cjs{dep}").into();
                    bindings.push((alias.clone(), exports.clone(), d.span, false));
                    for spec in &d.named {
                        cjs_named.push((spec.local.clone(), alias.clone(), spec.imported.clone()));
                    }
                }
            }
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
            // `const { A, B: C } = require("./cjs")` of statically known
            // exports: bind the names directly, like an import.
            StmtKind::Var { kind: VarKind::Const, decls, declare: false }
                if decls.len() == 1
                    && let Some(dep) = required_dep(module, decls[0].init.as_ref())
                    && let Some(statics) = cjs_static_exports(&g.modules[dep]) =>
            {
                match &decls[0].pat {
                    Pattern::Object { props, rest: None, .. }
                        if props.iter().all(|(k, p, d)| d.is_none() && matches!(p, Pattern::Ident(..)) && statics.iter().any(|(e, _)| e == k)) =>
                    {
                        for (k, p, _) in props {
                            let Pattern::Ident(local, _) = p else { unreachable!() };
                            let (_, l) = statics.iter().find(|(e, _)| e == k).unwrap();
                            bindings.push((local.clone(), ExportTarget::Local(dep, l.clone()), s.span, false));
                        }
                    }
                    Pattern::Ident(local, _) => {
                        // `const som = require("./som")`: `som.X` reads the binding.
                        for (e, l) in &statics {
                            bindings.push((format!("{local}.{e}").into(), ExportTarget::Local(dep, l.clone()), Span::default(), true));
                        }
                        cjs_ns.push(local.clone());
                        stmts.push(s.clone());
                    }
                    _ => stmts.push(s.clone()),
                }
            }
            _ => stmts.push(s.clone()),
        }
    }
    if !cjs_ns.is_empty() {
        let names = cjs_ns.clone();
        visit_exprs_mut(&mut stmts, &mut |e| {
            if let ExprKind::Member { obj, prop, optional: false } = &e.kind
                && let ExprKind::Ident(n) = &obj.kind
                && names.contains(n)
                && bindings_has(&bindings, &format!("{n}.{prop}"))
            {
                e.kind = ExprKind::Ident(format!("{n}.{prop}").into());
            }
        });
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
    // `require("x")`, `import("x")` and `import.meta`.
    let mut required: Vec<(Name, ExportTarget)> = Vec::new();
    let file = module.path.clone();
    visit_exprs_mut(&mut stmts, &mut |e| {
        let span = e.span;
        let mk = |kind: ExprKind| Expr { kind, span };
        match &e.kind {
            ExprKind::Call { callee, args, .. }
                if matches!(&callee.kind, ExprKind::Ident(n) if &**n == "require" || &**n == "import")
                    && let [ArrayElem::Expr(Expr { kind: ExprKind::Str(spec), .. })] = &args[..]
                    && let Some(&dep) = module.deps.get(spec) =>
            {
                let is_import = matches!(&callee.kind, ExprKind::Ident(n) if &**n == "import");
                let alias: Name = format!("\u{1}req{dep}").into();
                let target = if g.modules[dep].is_cjs { ExportTarget::Local(dep, CJS_EXPORTS.into()) } else { ExportTarget::Namespace(dep) };
                if !required.iter().any(|(a, _)| *a == alias) {
                    required.push((alias.clone(), target));
                }
                let value = mk(ExprKind::Ident(alias));
                e.kind = if is_import {
                    // import("x") → Promise.resolve(<namespace>)
                    ExprKind::Call {
                        callee: Box::new(mk(ExprKind::Member {
                            obj: Box::new(mk(ExprKind::Ident("Promise".into()))),
                            prop: "resolve".into(),
                            optional: false,
                        })),
                        targs: vec![],
                        args: vec![ArrayElem::Expr(value)],
                        optional: false,
                    }
                } else {
                    value.kind
                };
            }
            ExprKind::Member { obj, prop, .. } if &**prop == "meta" && matches!(&obj.kind, ExprKind::Ident(n) if &**n == "import") => {
                let dir = std::path::Path::new(&file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let field = |k: &str, v: String| ObjProp::Init {
                    key: PropKey::Name(k.into()),
                    value: mk(ExprKind::Str(v.into())),
                };
                e.kind = ExprKind::Object(vec![
                    field("url", format!("file://{file}")),
                    field("filename", file.clone()),
                    field("dirname", dir),
                ]);
            }
            ExprKind::Ident(n) => {
                if let Some((_, alias, prop)) = cjs_named.iter().find(|(l, _, _)| l == n) {
                    e.kind = ExprKind::Member {
                        obj: Box::new(mk(ExprKind::Ident(alias.clone()))),
                        prop: prop.clone(),
                        optional: false,
                    };
                }
            }
            _ => {}
        }
    });
    for (alias, target) in required {
        bindings.push((alias, target, Span::default(), false));
    }
    if module.is_cjs {
        stmts = wrap_commonjs(stmts, &file);
    }
    let _ = ImportOf::Namespace;
    Ok(Prepared { stmts, bindings })
}

/// A CommonJS module's body, with `module`, `exports`, `__filename` and
/// `__dirname` defined first and `module.exports` captured last.
fn wrap_commonjs(body: Vec<Stmt>, file: &str) -> Vec<Stmt> {
    let dir = std::path::Path::new(file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let head = format!(
        "var module: any = {{ exports: {{}} }}; var exports: any = module.exports; const __filename = {:?}; const __dirname = {:?};",
        file, dir
    );
    let tail = format!("const {} = module.exports;", "__rts_cjs_exports");
    let parse = |src: &str| crate::parser::parse_program(crate::lexer::tokenize(src).expect("wrapper lexes")).expect("wrapper parses");
    let mut out = parse(&head);
    out.extend(body);
    let mut t = parse(&tail);
    // Rename to the hidden name programs can't spell.
    if let StmtKind::Var { decls, .. } = &mut t[0].kind {
        decls[0].pat = Pattern::Ident(CJS_EXPORTS.into(), Span::default());
    }
    out.extend(t);
    out
}

fn bindings_has(b: &[(Name, ExportTarget, Span, bool)], name: &str) -> bool {
    b.iter().any(|(n, ..)| &**n == name)
}

/// The dependency a `require("x")` expression loads.
fn required_dep(module: &crate::module::Module, init: Option<&Expr>) -> Option<usize> {
    let Expr { kind: ExprKind::Call { callee, args, .. }, .. } = init? else { return None };
    if !matches!(&callee.kind, ExprKind::Ident(n) if &**n == "require") {
        return None;
    }
    let [ArrayElem::Expr(Expr { kind: ExprKind::Str(spec), .. })] = &args[..] else { return None };
    module.deps.get(spec).copied()
}

/// A CommonJS module's exports when they can be read off its top level, as
/// Node's cjs-module-lexer does: `exports.X = L`, `module.exports.X = L` and
/// `module.exports = { X: L, … }`, where `L` is a top-level class, function or
/// const. Returns (exported name, local name) pairs, or None when any write to
/// the exports is not of that form.
pub fn cjs_static_exports(m: &crate::module::Module) -> Option<Vec<(Name, Name)>> {
    if !m.is_cjs {
        return None;
    }
    let fixed = |n: &str| {
        m.stmts.iter().any(|s| match &s.kind {
            StmtKind::Class(c) => c.name.as_deref() == Some(n),
            StmtKind::Func(f) => f.name.as_deref() == Some(n),
            StmtKind::Var { kind: VarKind::Const, .. } => declares(s, n),
            _ => false,
        })
    };
    let is_exports = |e: &Expr| match &e.kind {
        ExprKind::Ident(n) => &**n == "exports",
        ExprKind::Member { obj, prop, .. } => &**prop == "exports" && matches!(&obj.kind, ExprKind::Ident(n) if &**n == "module"),
        _ => false,
    };
    let mut out: Vec<(Name, Name)> = Vec::new();
    for s in &m.stmts {
        let StmtKind::Expr(e) = &s.kind else { continue };
        let ExprKind::Assign(AssignOp::Assign, target, value) = &e.kind else { continue };
        match &target.kind {
            ExprKind::Member { obj, prop, .. } if is_exports(obj) => match &value.kind {
                ExprKind::Ident(l) if fixed(l) => out.push((prop.clone(), l.clone())),
                // `exports.f = () => …` and the like: a dynamic property.
                _ => {}
            },
            _ if is_exports(target) => match &value.kind {
                ExprKind::Object(props) => {
                    for p in props {
                        match p {
                            ObjProp::Init { key: PropKey::Name(k), value: Expr { kind: ExprKind::Ident(l), .. } } if fixed(l) => {
                                out.push((k.clone(), l.clone()))
                            }
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            },
            _ => {}
        }
    }
    Some(out)
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
