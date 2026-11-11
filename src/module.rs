//! Loading a program's modules: resolve `import` specifiers to files the way
//! Node does (relative paths, `node_modules`, `package.json` `exports`/`main`),
//! parse each file once, and order them for evaluation.
//!
//! The checker is whole-program: every module becomes a scope of the one
//! program, and an import binds the importing scope's name to the exporting
//! module's variable itself. So imports are live bindings at no run-time cost.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::*;
use crate::error::{CompileError, Sources, Span};
use crate::runtime::Value;
use crate::{lexer, parser};

pub struct Module {
    /// Resolved path (or `node:name` for a built-in module).
    pub path: String,
    pub stmts: Vec<Stmt>,
    /// Import specifier → index of the module it resolves to.
    pub deps: FxHashMap<Name, usize>,
}

pub struct ModuleGraph {
    pub modules: Vec<Module>,
    /// Evaluation order: every module after the modules it imports
    /// (depth-first post-order from the entry, as in Node).
    pub order: Vec<usize>,
}

/// Built-in modules (`node:fs` and friends), written in TypeScript.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    let name = name.strip_prefix("node:").unwrap_or(name);
    crate::node::MODULES.iter().find(|(n, _)| *n == name).map(|(_, src)| *src)
}

const EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "mjs", "cjs", "jsx", "json"];

/// Load `entry` and everything it imports.
pub fn load(entry: &Path, entry_src: String, sources: &mut Sources) -> Result<ModuleGraph, CompileError> {
    let mut loader = Loader {
        modules: Vec::new(),
        by_path: FxHashMap::default(),
        sources,
    };
    let entry_path = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    loader.add(entry_path.to_string_lossy().into_owned(), display_name(entry), entry_src)?;
    let mut i = 0;
    while i < loader.modules.len() {
        loader.resolve_deps(i)?;
        i += 1;
    }
    let mut order = Vec::new();
    let mut seen = vec![false; loader.modules.len()];
    post_order(&loader.modules, 0, &mut seen, &mut order);
    Ok(ModuleGraph {
        modules: loader.modules,
        order,
    })
}

fn display_name(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn post_order(ms: &[Module], m: usize, seen: &mut [bool], out: &mut Vec<usize>) {
    if seen[m] {
        return;
    }
    seen[m] = true;
    let mut deps: Vec<(usize, usize)> = Vec::new();
    // Visit dependencies in the order their imports appear.
    for (i, s) in ms[m].stmts.iter().enumerate() {
        for spec in specifiers(s) {
            if let Some(&d) = ms[m].deps.get(&spec) {
                deps.push((i, d));
            }
        }
    }
    for (_, d) in deps {
        post_order(ms, d, seen, out);
    }
    out.push(m);
}

/// The module specifiers a statement depends on.
pub fn specifiers(s: &Stmt) -> Vec<Name> {
    match &s.kind {
        StmtKind::Import(d) => vec![d.from.clone()],
        StmtKind::Export(e) => match &**e {
            ExportDecl::Named { from: Some(f), .. } | ExportDecl::All { from: f, .. } => vec![f.clone()],
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Specifiers of `import("x")` and `require("x")` calls with a literal argument.
fn call_specifiers(stmts: &mut [Stmt]) -> Vec<(Name, Span)> {
    let mut out = Vec::new();
    visit_exprs_mut(stmts, &mut |e| {
        if let ExprKind::Call { callee, args, .. } = &e.kind
            && let ExprKind::Ident(n) = &callee.kind
            && (&**n == "require" || &**n == "import")
            && let [ArrayElem::Expr(Expr { kind: ExprKind::Str(s), .. })] = &args[..]
        {
            out.push((s.clone(), e.span));
        }
    });
    out
}

struct Loader<'a> {
    modules: Vec<Module>,
    by_path: FxHashMap<String, usize>,
    sources: &'a mut Sources,
}

impl Loader<'_> {
    fn add(&mut self, key: String, display: String, src: String) -> Result<usize, CompileError> {
        if let Some(&i) = self.by_path.get(&key) {
            return Ok(i);
        }
        let src = if key.ends_with(".json") {
            format!("export default {src};")
        } else {
            src
        };
        let base = self.sources.add(display, src.clone());
        let toks = lexer::tokenize_at(&src, base)?;
        let stmts = parser::parse_program(toks)?;
        let i = self.modules.len();
        self.modules.push(Module {
            path: key.clone(),
            stmts,
            deps: FxHashMap::default(),
        });
        self.by_path.insert(key, i);
        Ok(i)
    }

    fn resolve_deps(&mut self, m: usize) -> Result<(), CompileError> {
        let mut specs: Vec<(Name, Span)> = Vec::new();
        for s in &self.modules[m].stmts {
            for spec in specifiers(s) {
                specs.push((spec, s.span));
            }
        }
        specs.extend(call_specifiers(&mut self.modules[m].stmts));
        let from = self.modules[m].path.clone();
        for (spec, span) in specs {
            if self.modules[m].deps.contains_key(&spec) {
                continue;
            }
            let d = self.load_specifier(&from, &spec, span)?;
            self.modules[m].deps.insert(spec, d);
        }
        Ok(())
    }

    fn load_specifier(&mut self, from: &str, spec: &str, span: Span) -> Result<usize, CompileError> {
        if let Some(src) = builtin_source(spec) {
            let name = format!("node:{}", spec.strip_prefix("node:").unwrap_or(spec));
            return self.add(name.clone(), name, src.to_string());
        }
        if spec.starts_with("node:") {
            return Err(CompileError::new(format!("built-in module '{spec}' is not supported"), span));
        }
        let dir = if from.starts_with("node:") {
            std::env::current_dir().unwrap_or_default()
        } else {
            Path::new(from).parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let Some(path) = resolve(&dir, spec) else {
            return Err(CompileError::new(format!("cannot find module '{spec}' (imported from {from})"), span));
        };
        let path = path.canonicalize().unwrap_or(path);
        let key = path.to_string_lossy().into_owned();
        if let Some(&i) = self.by_path.get(&key) {
            return Ok(i);
        }
        let src = std::fs::read_to_string(&path).map_err(|e| CompileError::new(format!("cannot read module '{key}': {e}"), span))?;
        let display = match std::env::current_dir().ok().and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf)) {
            Some(rel) => rel.to_string_lossy().into_owned(),
            None => key.clone(),
        };
        self.add(key, display, src)
    }
}

/// Resolve an import specifier from a module in `dir`, as Node and
/// TypeScript do: `./x.js` may name `./x.ts`.
pub fn resolve(dir: &Path, spec: &str) -> Option<PathBuf> {
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || spec == "." || spec == ".." {
        let p = if spec.starts_with('/') { PathBuf::from(spec) } else { dir.join(spec) };
        return resolve_file(&p).or_else(|| resolve_dir(&p));
    }
    if let Some(rest) = spec.strip_prefix("file://") {
        return resolve_file(Path::new(rest));
    }
    resolve_bare(dir, spec)
}

/// `p` as a file: exactly, with a TypeScript extension in place of a `.js`
/// one, or with an extension added.
fn resolve_file(p: &Path) -> Option<PathBuf> {
    let s = p.to_string_lossy();
    for (js, tss) in [(".js", &[".ts", ".tsx"][..]), (".mjs", &[".mts"]), (".cjs", &[".cts"]), (".jsx", &[".tsx"])] {
        if let Some(stem) = s.strip_suffix(js) {
            for ts in tss {
                let c = PathBuf::from(format!("{stem}{ts}"));
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    for ext in EXTENSIONS {
        let c = PathBuf::from(format!("{s}.{ext}"));
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// `p` as a directory: its `package.json` entry point, or `index.*`.
fn resolve_dir(p: &Path) -> Option<PathBuf> {
    if !p.is_dir() {
        return None;
    }
    if let Some(pkg) = read_package(p) {
        for field in ["module", "main"] {
            if let Some(Value::Str(m)) = pkg_field(&pkg, field) {
                let target = p.join(&*m.s);
                if let Some(f) = resolve_file(&target).or_else(|| resolve_dir_index(&target)) {
                    return Some(f);
                }
            }
        }
    }
    resolve_dir_index(p)
}

fn resolve_dir_index(p: &Path) -> Option<PathBuf> {
    resolve_file(&p.join("index"))
}

fn read_package(dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    crate::runtime::natives::parse_json(&text)
}

fn pkg_field(v: &Value, name: &str) -> Option<Value> {
    match v {
        Value::Obj(o) => o.get(name),
        _ => None,
    }
}

/// `pkg` or `@scope/pkg`, optionally followed by a subpath.
fn resolve_bare(dir: &Path, spec: &str) -> Option<PathBuf> {
    let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let mut name = parts.next()?.to_string();
    if spec.starts_with('@') {
        name = format!("{name}/{}", parts.next()?);
    }
    let sub = parts.next().map(|s| format!("./{s}")).unwrap_or_else(|| ".".into());
    for d in dir.ancestors() {
        let pkg_dir = d.join("node_modules").join(&name);
        if pkg_dir.is_dir() {
            return resolve_package(&pkg_dir, &sub);
        }
    }
    None
}

fn resolve_package(pkg_dir: &Path, sub: &str) -> Option<PathBuf> {
    if let Some(pkg) = read_package(pkg_dir)
        && let Some(exports) = pkg_field(&pkg, "exports")
    {
        let target = resolve_exports(&exports, sub)?;
        return resolve_file(&pkg_dir.join(target.trim_start_matches("./")));
    }
    if sub == "." {
        return resolve_dir(pkg_dir);
    }
    let p = pkg_dir.join(&sub[2..]);
    resolve_file(&p).or_else(|| resolve_dir(&p))
}

/// Look up `sub` (`.` or `./x`) in a package.json `exports` value.
fn resolve_exports(exports: &Value, sub: &str) -> Option<String> {
    match exports {
        Value::Str(s) if sub == "." => Some(s.s.to_string()),
        Value::Obj(o) => {
            let keys = o.keys();
            if keys.first().is_some_and(|k| k.starts_with('.')) {
                if let Some(v) = o.get(sub) {
                    return pick_condition(&v);
                }
                // Subpath patterns: "./*": "./dist/*.js"
                for k in keys {
                    if let Some((pre, post)) = k.split_once('*')
                        && sub.starts_with(pre)
                        && sub.ends_with(post)
                        && sub.len() >= pre.len() + post.len()
                    {
                        let star = &sub[pre.len()..sub.len() - post.len()];
                        let target = pick_condition(&o.get(&k)?)?;
                        return Some(target.replace('*', star));
                    }
                }
                None
            } else if sub == "." {
                pick_condition(exports)
            } else {
                None
            }
        }
        Value::Arr(_) if sub == "." => pick_condition(exports),
        _ => None,
    }
}

/// Pick a target from a conditional export (`{ "import": …, "default": … }`).
fn pick_condition(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.s.to_string()),
        Value::Obj(o) => {
            for cond in ["import", "node", "module", "default", "require"] {
                if let Some(t) = o.get(cond).and_then(|t| pick_condition(&t)) {
                    return Some(t);
                }
            }
            None
        }
        Value::Arr(a) => (0..a.len()).find_map(|i| pick_condition(&a.get(i))),
        _ => None,
    }
}

/// What an exported name refers to, found without type checking.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportTarget {
    /// A binding declared (or imported) in module `.0` under the name `.1`.
    Local(usize, Name),
    /// The namespace object of a module (`export * as ns`).
    Namespace(usize),
}

/// The export table of every module: name → target, before `export *`.
pub struct Exports {
    pub named: Vec<FxHashMap<Name, ExportTarget>>,
    pub star: Vec<Vec<usize>>,
}

impl Exports {
    /// Build the tables from the (unmodified) module statements.
    pub fn new(g: &ModuleGraph) -> Exports {
        let mut named = Vec::new();
        let mut star = Vec::new();
        for (mi, m) in g.modules.iter().enumerate() {
            let mut t: FxHashMap<Name, ExportTarget> = FxHashMap::default();
            let mut st = Vec::new();
            for s in &m.stmts {
                let StmtKind::Export(e) = &s.kind else { continue };
                match &**e {
                    ExportDecl::Decl(d) => {
                        for n in declared_names(d) {
                            t.insert(n.clone(), ExportTarget::Local(mi, n));
                        }
                    }
                    ExportDecl::DefaultDecl(d) => {
                        let n = declared_names(d).into_iter().next().unwrap_or_else(|| "default".into());
                        t.insert("default".into(), ExportTarget::Local(mi, n));
                    }
                    ExportDecl::DefaultExpr(_) => {
                        t.insert("default".into(), ExportTarget::Local(mi, "default".into()));
                    }
                    ExportDecl::Named { specs, from, .. } => {
                        for (local, exported) in specs {
                            let target = match from {
                                Some(f) => ExportTarget::Local(m.deps[f], local.clone()),
                                None => ExportTarget::Local(mi, local.clone()),
                            };
                            t.insert(exported.clone(), target);
                        }
                    }
                    ExportDecl::All { from, alias: Some(a), .. } => {
                        t.insert(a.clone(), ExportTarget::Namespace(m.deps[from]));
                    }
                    ExportDecl::All { from, alias: None, .. } => st.push(m.deps[from]),
                }
            }
            named.push(t);
            star.push(st);
        }
        Exports { named, star }
    }

    /// Follow re-exports: what `name` exported from module `m` ultimately is.
    /// A `Local` result names a declaration or an import in its module.
    pub fn resolve(&self, g: &ModuleGraph, m: usize, name: &str) -> Option<ExportTarget> {
        self.resolve_in(g, m, name, &mut Vec::new())
    }

    fn resolve_in(&self, g: &ModuleGraph, m: usize, name: &str, visiting: &mut Vec<(usize, Rc<str>)>) -> Option<ExportTarget> {
        if visiting.iter().any(|(vm, vn)| *vm == m && &**vn == name) {
            return None;
        }
        visiting.push((m, name.into()));
        let r = match self.named[m].get(name) {
            Some(ExportTarget::Local(lm, ln)) if *lm != m => self.resolve_in(g, *lm, ln, visiting),
            Some(ExportTarget::Local(lm, ln)) => match import_of(g, *lm, ln) {
                // An exported import: follow it to the exporting module.
                Some(ImportOf::Named(dm, dn)) => self.resolve_in(g, dm, &dn, visiting),
                Some(ImportOf::Namespace(dm)) => Some(ExportTarget::Namespace(dm)),
                None => Some(ExportTarget::Local(*lm, ln.clone())),
            },
            Some(t) => Some(t.clone()),
            None if name == "default" => None,
            None => self.star[m].iter().find_map(|&s| self.resolve_in(g, s, name, visiting)),
        };
        visiting.pop();
        r
    }

    /// Every name module `m` exports (for namespace objects), sorted.
    pub fn names(&self, m: usize) -> Vec<Name> {
        let mut out: Vec<Name> = Vec::new();
        let mut seen = vec![m];
        self.collect_names(m, &mut out, &mut seen, true);
        out.sort();
        out.dedup();
        out
    }

    fn collect_names(&self, m: usize, out: &mut Vec<Name>, seen: &mut Vec<usize>, top: bool) {
        for n in self.named[m].keys() {
            if top || &**n != "default" {
                out.push(n.clone());
            }
        }
        for &s in &self.star[m] {
            if !seen.contains(&s) {
                seen.push(s);
                self.collect_names(s, out, seen, false);
            }
        }
    }
}

pub enum ImportOf {
    Named(usize, Name),
    Namespace(usize),
}

/// If `local` is bound by an import in module `m`, where it comes from.
pub fn import_of(g: &ModuleGraph, m: usize, local: &str) -> Option<ImportOf> {
    let module = &g.modules[m];
    for s in &module.stmts {
        let StmtKind::Import(d) = &s.kind else { continue };
        let dep = module.deps[&d.from];
        if d.default.as_deref() == Some(local) {
            return Some(ImportOf::Named(dep, "default".into()));
        }
        if d.namespace.as_deref() == Some(local) {
            return Some(ImportOf::Namespace(dep));
        }
        if let Some(spec) = d.named.iter().find(|s| &*s.local == local) {
            return Some(ImportOf::Named(dep, spec.imported.clone()));
        }
    }
    None
}

/// Names a declaration statement binds.
pub fn declared_names(s: &Stmt) -> Vec<Name> {
    match &s.kind {
        StmtKind::Var { decls, .. } => {
            let mut out = Vec::new();
            for d in decls {
                pattern_names(&d.pat, &mut out);
            }
            out
        }
        StmtKind::Func(f) => f.name.iter().cloned().collect(),
        StmtKind::Class(c) => c.name.iter().cloned().collect(),
        StmtKind::Interface { name, .. } | StmtKind::TypeAlias { name, .. } | StmtKind::Enum { name, .. } => vec![name.clone()],
        _ => vec![],
    }
}

fn pattern_names(p: &Pattern, out: &mut Vec<Name>) {
    match p {
        Pattern::Ident(n, _) => out.push(n.clone()),
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
