//! Type checker: AST → typed HIR.
//!
//! Scopes are persistent (`Rc` linked), so function bodies can be checked
//! lazily in their declaration environment — on first use when a caller needs
//! an inferred return type, otherwise at the end of the enclosing block.

mod builtins;
mod expr;
mod stmt;

use std::cell::RefCell;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{self, Name, TypeExpr};
use crate::error::{CompileError, Span};
use crate::hir::*;
use crate::types::*;

pub type CResult<T> = Result<T, CompileError>;

pub const MAIN: FuncId = 0;

// ───────────────────────────── scopes ─────────────────────────────

#[derive(Debug, Clone)]
pub enum VB {
    Var(VarId),
    /// Function declaration: its closure variable and definition.
    Func(VarId, FuncId),
    Class(ClassId),
    Enum(EnumId),
    Builtin(BuiltinNs),
    /// A global built-in function (`parseInt`, `String`, …).
    NativeFn(crate::runtime::Native),
}

#[derive(Debug, Clone)]
pub enum TB {
    Alias(u32),
    Iface(u32),
    Class(ClassId),
    Enum(EnumId),
    TParam(TParamId),
}

#[derive(Debug)]
pub struct Scope {
    pub parent: Option<Rc<Scope>>,
    pub vals: RefCell<FxHashMap<Name, VB>>,
    pub types: RefCell<FxHashMap<Name, TB>>,
    /// Function owning variables declared here; None = module scope.
    pub owner: Option<FuncId>,
    /// Variables declared directly in this scope (in order).
    pub decls: RefCell<Vec<VarId>>,
}

impl Scope {
    pub fn new(parent: Option<Rc<Scope>>, owner: Option<FuncId>) -> Rc<Scope> {
        Rc::new(Scope {
            parent,
            vals: Default::default(),
            types: Default::default(),
            owner,
            decls: Default::default(),
        })
    }
}

// ───────────────────────────── definitions ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnKind {
    Normal,
    Arrow,
    Method,
    StaticMethod,
    Ctor,
    /// Method in an object literal (`this` = the object).
    ObjMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unchecked,
    Checking,
    Done,
}

pub struct FuncDef {
    pub name: Name,
    pub ast: Option<Rc<ast::Function>>,
    pub scope: Rc<Scope>,
    pub parent: Option<FuncId>,
    pub class: Option<ClassId>,
    pub kind: FnKind,
    pub contextual: Option<Rc<FuncType>>,
    pub tparams: Vec<TParamId>,
    tscope: Option<Rc<Scope>>,
    params_resolved: Option<Vec<FParam>>,
    pub ret: Option<Type>,
    state: State,
    pub hir: Option<HFunc>,
    pub free: Vec<VarId>,
    pub this_ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: Name,
    pub ty: Option<Type>,
    pub ty_expr: Option<TypeExpr>,
    pub init: Option<ast::Expr>,
    pub optional: bool,
}

pub struct ClassDef {
    pub name: Name,
    pub decl: Rc<ast::ClassDecl>,
    pub scope: Rc<Scope>,
    /// Scope with type parameters bound.
    pub cscope: Rc<Scope>,
    pub tparams: Vec<TParamId>,
    pub parent: Option<(ClassId, Vec<Type>)>,
    /// All fields (inherited first) — slot = index.
    pub fields: Vec<FieldDef>,
    pub own_field_start: usize,
    /// name → (function, vtable slot)
    pub methods: FxHashMap<Name, (FuncId, u32)>,
    pub vtable: Vec<(Name, FuncId)>,
    pub ctor: FuncId,
    pub statics: FxHashMap<Name, VB>,
    /// Accessor properties: name → (getter, setter) as (function, vtable slot).
    pub accessors: FxHashMap<Name, (Option<(FuncId, u32)>, Option<(FuncId, u32)>)>,
    pub resolved: bool,
    pub is_abstract: bool,
    pub prelude_error: bool,
}

pub struct IfaceDef {
    pub name: Name,
    pub tparams: Vec<TParamId>,
    pub scope: Rc<Scope>,
    pub extends: Vec<TypeExpr>,
    pub members: Vec<ast::TypeMember>,
    pub obj: Option<Rc<ObjType>>,
    resolving: bool,
}

pub struct AliasDef {
    pub name: Name,
    pub tparams: Vec<TParamId>,
    pub scope: Rc<Scope>,
    pub ty: TypeExpr,
    pub resolved: Option<Type>,
    resolving: bool,
}

pub struct EnumDef {
    pub name: Name,
    pub members: Vec<(Name, EnumVal)>,
}

#[derive(Debug, Clone)]
pub enum EnumVal {
    Num(f64),
    Str(Name),
}

pub struct TParamDef {
    pub name: Name,
    pub constraint: Option<Type>,
}

// ───────────────────────────── per-function context ─────────────────────────────

pub type Narrow = FxHashMap<String, Type>;

pub struct FnCtx {
    pub func: FuncId,
    pub scope: Rc<Scope>,
    pub fn_scope: Rc<Scope>,
    pub ret: Option<Type>,
    pub returns: Vec<Type>,
    pub narrow: Narrow,
    pub this_var: Option<VarId>,
    pub this_ty: Option<Type>,
    pub kind: FnKind,
    pub class: Option<ClassId>,
    /// Inside a constructor before/after super() — field inits to splice in.
    pub ctor_inits: Option<Vec<HStmt>>,
}

pub struct Checker {
    pub vars: Vec<VarInfo>,
    pub var_pending: Vec<bool>,
    pub var_const: Vec<bool>,
    /// Module-level variable referenced from a function other than main.
    pub var_outside_main: Vec<bool>,
    /// `const` with a literal initializer: reads are replaced by the literal.
    pub var_const_val: Vec<Option<HExpr>>,
    pub funcs: Vec<FuncDef>,
    pub classes: Vec<ClassDef>,
    pub ifaces: Vec<IfaceDef>,
    pub aliases: Vec<AliasDef>,
    pub enums: Vec<EnumDef>,
    pub tparams: Vec<TParamDef>,
    pub stack: Vec<FnCtx>,
    pub module: Rc<Scope>,
    assume: RefCell<Vec<(Type, Type)>>,
    /// Built-in generic placeholders (array element U, reduce accumulator).
    pub tp_u: TParamId,
    pub tp_t: TParamId,
    pub tp_k: TParamId,
    pub tp_v: TParamId,
    /// Destructuring statements for parameters (spliced into the body).
    param_stmts: Vec<HStmt>,
    /// Static field initializers, emitted at the class declaration.
    pub static_inits: Vec<(ClassId, VarId, ast::Expr)>,
    /// Narrowings inherited by the next function checked (closures see const narrowings).
    pub pending_narrow: Option<Narrow>,
    /// Experiment switch (`--no-types`): every variable, parameter, return value
    /// and field is treated as `any`, so the program runs on dynamic paths only.
    pub erase: bool,
}

impl TypeNames for Checker {
    fn class_name(&self, id: ClassId) -> String {
        self.classes[id as usize].name.to_string()
    }
    fn enum_name(&self, id: EnumId) -> String {
        self.enums[id as usize].name.to_string()
    }
    fn tparam_name(&self, id: TParamId) -> String {
        self.tparams[id as usize].name.to_string()
    }
    fn iface_name(&self, id: u32) -> String {
        self.ifaces[id as usize].name.to_string()
    }
}

pub fn err<T>(msg: impl Into<String>, span: Span) -> CResult<T> {
    Err(CompileError::new(msg, span))
}

/// Check a whole program (prelude statements first).
pub fn check_program(prelude: &[ast::Stmt], program: &[ast::Stmt], erase: bool) -> CResult<HProgram> {
    let module = Scope::new(None, None);
    let mut c = Checker {
        vars: Vec::new(),
        var_pending: Vec::new(),
        var_const: Vec::new(),
        var_outside_main: Vec::new(),
        var_const_val: Vec::new(),
        funcs: Vec::new(),
        classes: Vec::new(),
        ifaces: Vec::new(),
        aliases: Vec::new(),
        enums: Vec::new(),
        tparams: Vec::new(),
        stack: Vec::new(),
        module: module.clone(),
        assume: RefCell::new(Vec::new()),
        tp_u: 0,
        tp_t: 0,
        tp_k: 0,
        tp_v: 0,
        param_stmts: Vec::new(),
        static_inits: Vec::new(),
        pending_narrow: None,
        erase,
    };
    c.tp_u = c.new_tparam("U", None);
    c.tp_t = c.new_tparam("T", None);
    c.tp_k = c.new_tparam("K", None);
    c.tp_v = c.new_tparam("V", None);
    builtins::install_globals(&module);
    // Function 0 is main.
    c.funcs.push(FuncDef {
        name: "<main>".into(),
        ast: None,
        scope: module.clone(),
        parent: None,
        class: None,
        kind: FnKind::Normal,
        contextual: None,
        tparams: vec![],
        tscope: None,
        params_resolved: Some(vec![]),
        ret: Some(Type::Void),
        state: State::Checking,
        hir: None,
        free: vec![],
        this_ty: None,
        span: Span::default(),
    });
    c.stack.push(FnCtx {
        func: MAIN,
        scope: module.clone(),
        fn_scope: module.clone(),
        ret: None,
        returns: vec![],
        narrow: Narrow::default(),
        this_var: None,
        this_ty: None,
        kind: FnKind::Normal,
        class: None,
        ctor_inits: None,
    });
    let all: Vec<ast::Stmt> = prelude.iter().chain(program.iter()).cloned().collect();
    let body = c.check_block_in(&all, module.clone())?;
    c.stack.pop();
    // Module variables only used by top-level code live in main's registers.
    let mut demoted = Vec::new();
    for v in module.decls.borrow().iter() {
        let i = *v as usize;
        if c.vars[i].owner.is_none() && !c.var_outside_main[i] {
            c.vars[i].owner = Some(MAIN);
            demoted.push(*v);
        }
    }
    c.funcs[MAIN as usize].hir = Some(HFunc {
        name: "<main>".into(),
        body: vec![HStmt::Block(body, demoted)],
        ..Default::default()
    });
    c.funcs[MAIN as usize].state = State::Done;
    // Make sure every function and class member has been checked.
    let mut i = 0;
    while i < c.funcs.len() {
        c.ensure_checked(i as FuncId)?;
        i += 1;
    }
    c.finish()
}

impl Checker {
    pub fn new_tparam(&mut self, name: &str, constraint: Option<Type>) -> TParamId {
        self.tparams.push(TParamDef {
            name: name.into(),
            constraint,
        });
        self.tparams.len() as TParamId - 1
    }

    pub fn show(&self, t: &Type) -> String {
        format!("{}", Disp(t, self))
    }

    pub fn ctx(&self) -> &FnCtx {
        self.stack.last().unwrap()
    }
    pub fn ctx_mut(&mut self) -> &mut FnCtx {
        self.stack.last_mut().unwrap()
    }

    pub fn new_var(&mut self, name: Name, ty: Option<Type>, is_const: bool, scope: &Rc<Scope>) -> VarId {
        let id = self.vars.len() as VarId;
        let ty = if self.erase { Some(Type::Any) } else { ty };
        self.var_pending.push(ty.is_none());
        self.var_const.push(is_const);
        self.var_outside_main.push(false);
        self.var_const_val.push(None);
        self.vars.push(VarInfo {
            name,
            ty: ty.unwrap_or(Type::Any),
            owner: scope.owner,
            captured: false,
        });
        scope.decls.borrow_mut().push(id);
        id
    }

    pub fn push_scope(&mut self) -> Rc<Scope> {
        let cur = self.ctx().scope.clone();
        let owner = Some(self.ctx().func);
        let s = Scope::new(Some(cur), owner);
        self.ctx_mut().scope = s.clone();
        s
    }
    pub fn pop_scope(&mut self) {
        let p = self.ctx().scope.parent.clone().unwrap();
        self.ctx_mut().scope = p;
    }

    pub fn lookup_val(&self, name: &str) -> Option<VB> {
        let mut s = Some(self.ctx().scope.clone());
        while let Some(sc) = s {
            if let Some(b) = sc.vals.borrow().get(name) {
                return Some(b.clone());
            }
            s = sc.parent.clone();
        }
        None
    }

    fn lookup_type_in(&self, scope: &Rc<Scope>, name: &str) -> Option<TB> {
        let mut s = Some(scope.clone());
        while let Some(sc) = s {
            if let Some(b) = sc.types.borrow().get(name) {
                return Some(b.clone());
            }
            s = sc.parent.clone();
        }
        None
    }

    /// Record a reference to variable `v` from the current function: marks
    /// captures and threads the variable through intermediate closures.
    pub fn note_var_use(&mut self, v: VarId) {
        let owner = self.vars[v as usize].owner;
        let cur = self.ctx().func;
        let Some(owner) = owner else {
            if cur != MAIN {
                self.var_outside_main[v as usize] = true;
            }
            return;
        };
        if cur == owner {
            return;
        }
        self.vars[v as usize].captured = true;
        let mut f = Some(cur);
        while let Some(fi) = f {
            if fi == owner {
                break;
            }
            let fd = &mut self.funcs[fi as usize];
            if !fd.free.contains(&v) {
                fd.free.push(v);
            }
            f = fd.parent;
        }
    }

    // ───────────────────────────── type resolution ─────────────────────────────

    pub fn resolve_type(&mut self, te: &TypeExpr) -> CResult<Type> {
        let scope = self.ctx().scope.clone();
        self.resolve_type_in(te, &scope)
    }

    pub fn resolve_type_in(&mut self, te: &TypeExpr, scope: &Rc<Scope>) -> CResult<Type> {
        Ok(match te {
            TypeExpr::Array(t) => Type::array(self.resolve_type_in(t, scope)?),
            TypeExpr::Tuple(ts) => {
                let ts: CResult<Vec<Type>> = ts.iter().map(|t| self.resolve_type_in(t, scope)).collect();
                Type::Tuple(ts?.into())
            }
            TypeExpr::Union(ts) => {
                let ts: CResult<Vec<Type>> = ts.iter().map(|t| self.resolve_type_in(t, scope)).collect();
                Type::union(ts?)
            }
            TypeExpr::Intersection(ts) => {
                // Intersections of object types merge their properties.
                let mut props: Vec<Prop> = Vec::new();
                let mut index = None;
                let mut other = None;
                for t in ts {
                    let t = self.resolve_type_in(t, scope)?;
                    if let Some(o) = self.obj_view(&t) {
                        for p in &o.props {
                            if !props.iter().any(|q| q.name == p.name) {
                                props.push(p.clone());
                            }
                        }
                        if o.index.is_some() {
                            index = o.index.clone();
                        }
                    } else {
                        other = Some(t);
                    }
                }
                if let Some(t) = other
                    && props.is_empty()
                {
                    t
                } else {
                    Type::Object(Rc::new(ObjType {
                        props,
                        index,
                        call: None,
                    }))
                }
            }
            TypeExpr::Func { tparams, params, ret } => {
                let fs = Scope::new(Some(scope.clone()), scope.owner);
                let mut tps = Vec::new();
                for tp in tparams {
                    let c = match &tp.constraint {
                        Some(c) => Some(self.resolve_type_in(c, scope)?),
                        None => None,
                    };
                    let id = self.new_tparam(&tp.name, c);
                    fs.types.borrow_mut().insert(tp.name.clone(), TB::TParam(id));
                    tps.push(id);
                }
                let mut ps = Vec::new();
                for p in params {
                    let mut ty = match &p.ty {
                        Some(t) => self.resolve_type_in(t, &fs)?,
                        None => Type::Any,
                    };
                    if p.optional {
                        ty = Type::union([ty, Type::Undefined]);
                    }
                    ps.push(FParam {
                        name: pat_name(&p.pat),
                        ty,
                        optional: p.optional || p.default.is_some(),
                        rest: p.rest,
                    });
                }
                let ret = self.resolve_type_in(ret, &fs)?;
                Type::Func(Rc::new(FuncType {
                    tparams: tps,
                    params: ps,
                    ret,
                }))
            }
            TypeExpr::Object(members) => {
                let o = self.resolve_members(members, scope)?;
                Type::Object(Rc::new(o))
            }
            TypeExpr::StrLit(s) => Type::StrLit(s.clone()),
            TypeExpr::NumLit(n) => Type::NumLit(*n),
            TypeExpr::BoolLit(b) => Type::BoolLit(*b),
            TypeExpr::KeyOf(t) => {
                let t = self.resolve_type_in(t, scope)?;
                match self.obj_view(&t) {
                    Some(o) if o.index.is_none() => Type::union(o.props.iter().map(|p| Type::StrLit(p.name.clone()))),
                    _ => Type::String,
                }
            }
            TypeExpr::TypeOf(name, span) => {
                // Only simple variables are supported.
                let saved = self.ctx().scope.clone();
                self.ctx_mut().scope = scope.clone();
                let r = match self.lookup_val(name) {
                    Some(VB::Var(v)) | Some(VB::Func(v, _)) if !self.var_pending[v as usize] => {
                        Ok(self.vars[v as usize].ty.clone())
                    }
                    Some(VB::Func(_, f)) => self.func_type(f).map(Type::Func),
                    Some(VB::Class(c)) => Ok(Type::ClassCtor(c)),
                    Some(VB::Enum(e)) => Ok(Type::EnumObj(e)),
                    _ => err(format!("cannot resolve 'typeof {name}'"), *span),
                };
                self.ctx_mut().scope = saved;
                r?
            }
            TypeExpr::Ref { name, args, span } => self.resolve_ref(name, args, *span, scope)?,
        })
    }

    fn resolve_members(&mut self, members: &[ast::TypeMember], scope: &Rc<Scope>) -> CResult<ObjType> {
        let mut o = ObjType::default();
        for m in members {
            match m {
                ast::TypeMember::Prop { name, optional, ty, .. } => {
                    let mut t = self.resolve_type_in(ty, scope)?;
                    if *optional {
                        t = Type::union([t, Type::Undefined]);
                    }
                    o.props.retain(|p| p.name != *name);
                    o.props.push(Prop {
                        name: name.clone(),
                        ty: t,
                        optional: *optional,
                    });
                }
                ast::TypeMember::Index { value, .. } => o.index = Some(self.resolve_type_in(value, scope)?),
                ast::TypeMember::Call(f) => {
                    if let Type::Func(ft) = self.resolve_type_in(f, scope)? {
                        o.call = Some(ft);
                    }
                }
            }
        }
        Ok(o)
    }

    fn resolve_ref(&mut self, name: &Name, args: &[TypeExpr], span: Span, scope: &Rc<Scope>) -> CResult<Type> {
        let mut targs = Vec::new();
        for a in args {
            targs.push(self.resolve_type_in(a, scope)?);
        }
        let arg = |i: usize| targs.get(i).cloned().unwrap_or(Type::Any);
        if let Some(tb) = self.lookup_type_in(scope, name) {
            return Ok(match tb {
                TB::TParam(id) => Type::Param(id),
                TB::Class(cid) => {
                    let n = self.classes[cid as usize].tparams.len();
                    let mut a = targs.clone();
                    a.resize(n, Type::Any);
                    Type::Class(cid, a.into())
                }
                TB::Iface(id) => {
                    let n = self.ifaces[id as usize].tparams.len();
                    let mut a = targs.clone();
                    a.resize(n, Type::Any);
                    Type::Iface(id, a.into())
                }
                TB::Enum(eid) => self.enum_type(eid),
                TB::Alias(id) => self.alias_type(id, &targs, span)?,
            });
        }
        Ok(match &**name {
            "number" => Type::Number,
            "string" => Type::String,
            "boolean" => Type::Boolean,
            "void" => Type::Void,
            "null" => Type::Null,
            "undefined" => Type::Undefined,
            "any" => Type::Any,
            "unknown" => Type::Unknown,
            "never" => Type::Never,
            "object" | "Object" | "Function" | "this" => Type::Any,
            "Array" | "ReadonlyArray" => Type::array(arg(0)),
            "Map" | "ReadonlyMap" => Type::Map(Rc::new(arg(0)), Rc::new(arg(1))),
            "Set" | "ReadonlySet" => Type::Set(Rc::new(arg(0))),
            "Record" => Type::Object(Rc::new(ObjType {
                props: match arg(0) {
                    Type::Union(ts) if ts.iter().all(|t| matches!(t, Type::StrLit(_))) => ts
                        .iter()
                        .map(|t| Prop {
                            name: match t {
                                Type::StrLit(s) => s.clone(),
                                _ => unreachable!(),
                            },
                            ty: arg(1),
                            optional: false,
                        })
                        .collect(),
                    _ => vec![],
                },
                index: Some(arg(1)),
                call: None,
            })),
            "Partial" | "Required" | "Readonly" => {
                let t = arg(0);
                if &**name == "Readonly" {
                    return Ok(t);
                }
                match self.obj_view(&t) {
                    Some(o) => {
                        let opt = &**name == "Partial";
                        Type::Object(Rc::new(ObjType {
                            props: o
                                .props
                                .iter()
                                .map(|p| Prop {
                                    name: p.name.clone(),
                                    ty: if opt {
                                        Type::union([p.ty.clone(), Type::Undefined])
                                    } else {
                                        p.ty.non_null()
                                    },
                                    optional: opt,
                                })
                                .collect(),
                            index: o.index.clone(),
                            call: None,
                        }))
                    }
                    None => t,
                }
            }
            "NonNullable" => arg(0).non_null(),
            "ReturnType" => match arg(0) {
                Type::Func(f) => f.ret.clone(),
                _ => Type::Any,
            },
            "Promise" | "PromiseLike" => return err("Promise types are not supported yet (no async)", span),
            "symbol" | "bigint" | "Symbol" => return err(format!("'{name}' is not supported"), span),
            "Iterable" | "IterableIterator" | "Iterator" | "ArrayLike" => Type::array(arg(0)),
            _ => return err(format!("cannot find type '{name}'"), span),
        })
    }

    pub fn enum_type(&self, eid: EnumId) -> Type {
        let e = &self.enums[eid as usize];
        if e.members.iter().all(|(_, v)| matches!(v, EnumVal::Num(_))) {
            Type::Enum(eid)
        } else {
            Type::union(e.members.iter().map(|(_, v)| match v {
                EnumVal::Num(_) => Type::Number,
                EnumVal::Str(s) => Type::StrLit(s.clone()),
            }))
        }
    }

    fn alias_type(&mut self, id: u32, targs: &[Type], span: Span) -> CResult<Type> {
        let a = &self.aliases[id as usize];
        if a.resolving {
            return err(
                format!("type alias '{}' circularly references itself (use an interface)", a.name),
                span,
            );
        }
        let base = match &a.resolved {
            Some(t) => t.clone(),
            None => {
                let (te, scope) = (a.ty.clone(), a.scope.clone());
                self.aliases[id as usize].resolving = true;
                let t = self.resolve_type_in(&te, &scope);
                self.aliases[id as usize].resolving = false;
                let t = t?;
                self.aliases[id as usize].resolved = Some(t.clone());
                t
            }
        };
        let tps = self.aliases[id as usize].tparams.clone();
        if tps.is_empty() {
            return Ok(base);
        }
        let map: FxHashMap<TParamId, Type> = tps
            .iter()
            .enumerate()
            .map(|(i, t)| (*t, targs.get(i).cloned().unwrap_or(Type::Any)))
            .collect();
        Ok(self.subst(&base, &map))
    }

    pub fn iface_obj(&mut self, id: u32) -> Rc<ObjType> {
        if let Some(o) = &self.ifaces[id as usize].obj {
            return o.clone();
        }
        if self.ifaces[id as usize].resolving {
            return Rc::new(ObjType::default());
        }
        self.ifaces[id as usize].resolving = true;
        let (scope, members, extends) = {
            let i = &self.ifaces[id as usize];
            (i.scope.clone(), i.members.clone(), i.extends.clone())
        };
        let mut o = ObjType::default();
        for e in &extends {
            if let Ok(t) = self.resolve_type_in(e, &scope)
                && let Some(base) = self.obj_view(&t)
            {
                o.props.extend(base.props.iter().cloned());
                if base.index.is_some() {
                    o.index = base.index.clone();
                }
            }
        }
        // Errors in member types surface when used; resolve leniently here.
        match self.resolve_members(&members, &scope) {
            Ok(own) => {
                for p in own.props {
                    o.props.retain(|q| q.name != p.name);
                    o.props.push(p);
                }
                if own.index.is_some() {
                    o.index = own.index;
                }
                if own.call.is_some() {
                    o.call = own.call;
                }
            }
            Err(_) => {}
        }
        let o = Rc::new(o);
        self.ifaces[id as usize].obj = Some(o.clone());
        self.ifaces[id as usize].resolving = false;
        o
    }

    /// Structural view of a type (object literal types, interfaces, classes).
    pub fn obj_view(&mut self, t: &Type) -> Option<Rc<ObjType>> {
        match t {
            Type::Object(o) => Some(o.clone()),
            Type::Iface(id, args) => {
                let o = self.iface_obj(*id);
                let tps = self.ifaces[*id as usize].tparams.clone();
                if tps.is_empty() {
                    return Some(o);
                }
                let map: FxHashMap<TParamId, Type> = tps.iter().cloned().zip(args.iter().cloned()).collect();
                match self.subst(&Type::Object(o), &map) {
                    Type::Object(o) => Some(o),
                    _ => None,
                }
            }
            Type::Class(cid, args) => {
                let _ = self.ensure_class(*cid);
                let map = self.class_subst(*cid, args);
                let c = &self.classes[*cid as usize];
                let mut props: Vec<Prop> = c
                    .fields
                    .iter()
                    .map(|f| Prop {
                        name: f.name.clone(),
                        ty: f.ty.clone().unwrap_or(Type::Any),
                        optional: f.optional,
                    })
                    .collect();
                let accessors: Vec<(Name, Option<(FuncId, u32)>)> = c.accessors.iter().map(|(n, (g, _))| (n.clone(), *g)).collect();
                let methods: Vec<(Name, FuncId)> = c.vtable.iter().filter(|(n, _)| !n.contains(' ')).cloned().collect();
                for (n, g) in accessors {
                    let ty = match g {
                        Some((f, _)) => self.func_ret(f).unwrap_or(Type::Any),
                        None => Type::Any,
                    };
                    props.push(Prop {
                        name: n,
                        ty,
                        optional: false,
                    });
                }
                for (n, f) in methods {
                    if let Ok(ft) = self.func_type(f) {
                        props.push(Prop {
                            name: n,
                            ty: Type::Func(ft),
                            optional: false,
                        });
                    }
                }
                let o = Type::Object(Rc::new(ObjType {
                    props,
                    index: None,
                    call: None,
                }));
                match self.subst(&o, &map) {
                    Type::Object(o) => Some(o),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn class_subst(&self, cid: ClassId, args: &[Type]) -> FxHashMap<TParamId, Type> {
        self.classes[cid as usize]
            .tparams
            .iter()
            .cloned()
            .zip(args.iter().cloned().chain(std::iter::repeat(Type::Any)))
            .collect()
    }

    pub fn subst(&self, t: &Type, map: &FxHashMap<TParamId, Type>) -> Type {
        if map.is_empty() {
            return t.clone();
        }
        match t {
            Type::Param(id) => map.get(id).cloned().unwrap_or_else(|| t.clone()),
            Type::Array(e) => Type::array(self.subst(e, map)),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| self.subst(x, map)).collect()),
            Type::Union(ts) => Type::union(ts.iter().map(|x| self.subst(x, map))),
            Type::Func(f) => Type::Func(Rc::new(self.subst_fn(f, map))),
            Type::Object(o) => Type::Object(Rc::new(ObjType {
                props: o
                    .props
                    .iter()
                    .map(|p| Prop {
                        name: p.name.clone(),
                        ty: self.subst(&p.ty, map),
                        optional: p.optional,
                    })
                    .collect(),
                index: o.index.as_ref().map(|t| self.subst(t, map)),
                call: o.call.as_ref().map(|f| Rc::new(self.subst_fn(f, map))),
            })),
            Type::Class(c, args) => Type::Class(*c, args.iter().map(|x| self.subst(x, map)).collect()),
            Type::Iface(c, args) => Type::Iface(*c, args.iter().map(|x| self.subst(x, map)).collect()),
            Type::Map(k, v) => Type::Map(Rc::new(self.subst(k, map)), Rc::new(self.subst(v, map))),
            Type::Set(k) => Type::Set(Rc::new(self.subst(k, map))),
            t => t.clone(),
        }
    }

    pub fn subst_fn(&self, f: &FuncType, map: &FxHashMap<TParamId, Type>) -> FuncType {
        FuncType {
            tparams: f.tparams.iter().filter(|t| !map.contains_key(t)).cloned().collect(),
            params: f
                .params
                .iter()
                .map(|p| FParam {
                    name: p.name.clone(),
                    ty: self.subst(&p.ty, map),
                    optional: p.optional,
                    rest: p.rest,
                })
                .collect(),
            ret: self.subst(&f.ret, map),
        }
    }

    // ───────────────────────────── assignability ─────────────────────────────

    pub fn assignable(&mut self, src: &Type, dst: &Type) -> bool {
        if src == dst {
            return true;
        }
        match (src, dst) {
            (_, Type::Any) | (Type::Any, _) | (_, Type::Unknown) | (Type::Never, _) => return true,
            (Type::Undefined, Type::Void) => return true,
            (Type::Union(ss), _) => return ss.clone().iter().all(|s| self.assignable(s, dst)),
            (_, Type::Union(ds)) => {
                if ds.clone().iter().any(|d| self.assignable(src, d)) {
                    return true;
                }
                // boolean → true | false
                if *src == Type::Boolean
                    && ds.contains(&Type::BoolLit(true))
                    && ds.contains(&Type::BoolLit(false))
                {
                    return true;
                }
                return false;
            }
            (Type::StrLit(_), Type::String) => return true,
            (Type::NumLit(_), Type::Number) | (Type::NumLit(_), Type::Enum(_)) => return true,
            (Type::BoolLit(_), Type::Boolean) => return true,
            (Type::Enum(_), Type::Number) | (Type::Number, Type::Enum(_)) => return true,
            (Type::Param(a), _) => {
                if let Type::Param(b) = dst {
                    return a == b;
                }
                return match self.tparams[*a as usize].constraint.clone() {
                    Some(c) => self.assignable(&c, dst),
                    None => false,
                };
            }
            (_, Type::Param(_)) => return false,
            (Type::Array(a), Type::Array(b)) => return self.assignable(&a.clone(), &b.clone()),
            (Type::Tuple(ts), Type::Array(b)) => {
                let b = (**b).clone();
                return ts.clone().iter().all(|t| self.assignable(t, &b));
            }
            (Type::Tuple(a), Type::Tuple(b)) => {
                return a.len() == b.len() && a.clone().iter().zip(b.clone().iter()).all(|(x, y)| self.assignable(x, y));
            }
            (Type::Array(a), Type::Tuple(_)) if **a == Type::Any => return true,
            (Type::Map(k1, v1), Type::Map(k2, v2)) => {
                return self.assignable(&k1.clone(), &k2.clone()) && self.assignable(&v1.clone(), &v2.clone());
            }
            (Type::Set(a), Type::Set(b)) => return self.assignable(&a.clone(), &b.clone()),
            (Type::Func(f), Type::Func(g)) => return self.fn_assignable(&f.clone(), &g.clone()),
            (Type::ClassCtor(a), Type::ClassCtor(b)) => return a == b,
            (Type::Class(a, aa), Type::Class(b, ba)) => {
                if a == b {
                    return aa.clone().iter().zip(ba.clone().iter()).all(|(x, y)| self.assignable(x, y));
                }
                // Subclass?
                let mut cur = Some((*a, aa.to_vec()));
                while let Some((c, args)) = cur {
                    if c == *b {
                        return args.iter().zip(ba.clone().iter()).all(|(x, y)| self.assignable(x, y));
                    }
                    let _ = self.ensure_class(c);
                    let map = self.class_subst(c, &args);
                    cur = self.classes[c as usize]
                        .parent
                        .clone()
                        .map(|(p, pa)| (p, pa.iter().map(|t| self.subst(t, &map)).collect()));
                }
                // Fall through to structural check.
            }
            _ => {}
        }
        // Structural: dst object-like.
        let structural_dst = matches!(dst, Type::Object(_) | Type::Iface(..) | Type::Class(..));
        if !structural_dst {
            return false;
        }
        let pair = (src.clone(), dst.clone());
        if self.assume.borrow().contains(&pair) {
            return true;
        }
        let Some(d) = self.obj_view(dst) else { return false };
        // A function can satisfy an object type with only a call signature.
        if let (Type::Func(f), Some(call)) = (src, &d.call) {
            return self.fn_assignable(&f.clone(), &call.clone());
        }
        let Some(s) = self.obj_view(src) else { return false };
        self.assume.borrow_mut().push(pair);
        let mut ok = true;
        for p in &d.props {
            match s.prop(&p.name) {
                Some(sp) => {
                    if !self.assignable(&sp.ty.clone(), &p.ty) {
                        ok = false;
                        break;
                    }
                }
                None => {
                    if !p.optional {
                        // Index signatures in the source satisfy anything.
                        if !s.index.as_ref().is_some_and(|it| {
                            let it = it.clone();
                            self.assignable(&it, &p.ty)
                        }) {
                            ok = false;
                            break;
                        }
                    }
                }
            }
        }
        if ok && let Some(di) = &d.index {
            for p in &s.props {
                if !self.assignable(&p.ty.clone(), di) {
                    ok = false;
                    break;
                }
            }
        }
        self.assume.borrow_mut().pop();
        ok
    }

    fn fn_assignable(&mut self, f: &FuncType, g: &FuncType) -> bool {
        // Generic signatures: rename f's type parameters to g's (or erase them).
        if !f.tparams.is_empty() {
            let map: FxHashMap<TParamId, Type> = f
                .tparams
                .iter()
                .enumerate()
                .map(|(i, t)| (*t, g.tparams.get(i).map_or(Type::Any, |u| Type::Param(*u))))
                .collect();
            let f2 = self.subst_fn(f, &map);
            let f2 = FuncType { tparams: vec![], ..f2 };
            return self.fn_assignable(&f2, g);
        }
        // Fewer parameters are fine; parameters are checked bivariantly (as TS does for methods).
        let fp: Vec<&FParam> = f.params.iter().filter(|p| !p.rest).collect();
        let gp: Vec<&FParam> = g.params.iter().filter(|p| !p.rest).collect();
        if fp.iter().filter(|p| !p.optional).count() > gp.len() && !g.params.iter().any(|p| p.rest) {
            return false;
        }
        for (a, b) in fp.iter().zip(gp.iter()) {
            if !self.assignable(&b.ty, &a.ty) && !self.assignable(&a.ty, &b.ty) {
                return false;
            }
        }
        matches!(g.ret, Type::Void) || self.assignable(&f.ret, &g.ret)
    }

    // ───────────────────────────── functions ─────────────────────────────

    pub fn new_func(
        &mut self,
        name: Name,
        ast: Option<Rc<ast::Function>>,
        kind: FnKind,
        class: Option<ClassId>,
        contextual: Option<Rc<FuncType>>,
        scope: Rc<Scope>,
        this_ty: Option<Type>,
    ) -> FuncId {
        let id = self.funcs.len() as FuncId;
        let span = ast.as_ref().map_or(Span::default(), |a| a.span);
        let parent = scope.owner;
        self.funcs.push(FuncDef {
            name,
            ast,
            scope,
            parent,
            class,
            kind,
            contextual,
            tparams: vec![],
            tscope: None,
            params_resolved: None,
            ret: None,
            state: State::Unchecked,
            hir: None,
            free: vec![],
            this_ty,
            span,
        });
        id
    }

    /// Scope with the function's type parameters bound (created once).
    fn func_tscope(&mut self, f: FuncId) -> CResult<Rc<Scope>> {
        if let Some(s) = &self.funcs[f as usize].tscope {
            return Ok(s.clone());
        }
        let fd = &self.funcs[f as usize];
        let base = fd.scope.clone();
        let s = Scope::new(Some(base.clone()), Some(f));
        let mut ids = Vec::new();
        if let Some(a) = fd.ast.clone() {
            for tp in &a.tparams {
                let c = match &tp.constraint {
                    Some(c) => Some(self.resolve_type_in(c, &base)?),
                    None => None,
                };
                let id = self.new_tparam(&tp.name, c);
                s.types.borrow_mut().insert(tp.name.clone(), TB::TParam(id));
                ids.push(id);
            }
        }
        let fd = &mut self.funcs[f as usize];
        fd.tparams = ids;
        fd.tscope = Some(s.clone());
        Ok(s)
    }

    pub fn func_params(&mut self, f: FuncId) -> CResult<Vec<FParam>> {
        if let Some(p) = &self.funcs[f as usize].params_resolved {
            return Ok(p.clone());
        }
        let ts = self.func_tscope(f)?;
        let fd = &self.funcs[f as usize];
        let Some(a) = fd.ast.clone() else {
            return Ok(vec![]);
        };
        let ctx = fd.contextual.clone();
        let mut out = Vec::new();
        for (i, p) in a.params.iter().enumerate() {
            let cty = ctx.as_ref().and_then(|c| {
                if p.rest {
                    // Contextual rest: collect remaining contextual params as an array.
                    c.params.get(i).map(|cp| if cp.rest { cp.ty.clone() } else { Type::array(cp.ty.clone()) })
                } else {
                    c.params.get(i).map(|cp| if cp.rest {
                        match &cp.ty {
                            Type::Array(e) => (**e).clone(),
                            t => t.clone(),
                        }
                    } else {
                        cp.ty.clone()
                    })
                }
            });
            let mut ty = match (&p.ty, cty) {
                (Some(t), _) => self.resolve_type_in(t, &ts)?,
                (None, Some(c)) => c,
                (None, None) => match &p.default {
                    Some(d) => self.literal_type(d).unwrap_or(Type::Any),
                    None if p.rest => Type::array(Type::Any),
                    None => Type::Any,
                },
            };
            if p.optional && p.default.is_none() {
                ty = Type::union([ty, Type::Undefined]);
            }
            if self.erase {
                ty = if p.rest { Type::array(Type::Any) } else { Type::Any };
            }
            out.push(FParam {
                name: pat_name(&p.pat),
                ty,
                optional: p.optional || p.default.is_some(),
                rest: p.rest,
            });
        }
        self.funcs[f as usize].params_resolved = Some(out.clone());
        Ok(out)
    }

    /// Cheap type of a literal default value (for unannotated parameters).
    fn literal_type(&self, e: &ast::Expr) -> Option<Type> {
        Some(match &e.kind {
            ast::ExprKind::Num(_) => Type::Number,
            ast::ExprKind::Str(_) | ast::ExprKind::Template { .. } => Type::String,
            ast::ExprKind::Bool(_) => Type::Boolean,
            ast::ExprKind::Unary(ast::UnaryOp::Neg, _) => Type::Number,
            ast::ExprKind::Array(_) => Type::array(Type::Any),
            _ => return None,
        })
    }

    /// The function's full type; checks the body if the return type must be inferred.
    pub fn func_type(&mut self, f: FuncId) -> CResult<Rc<FuncType>> {
        let params = self.func_params(f)?;
        let ret = self.func_ret(f)?;
        Ok(Rc::new(FuncType {
            tparams: self.funcs[f as usize].tparams.clone(),
            params,
            ret,
        }))
    }

    pub fn func_ret(&mut self, f: FuncId) -> CResult<Type> {
        if let Some(r) = &self.funcs[f as usize].ret {
            return Ok(r.clone());
        }
        let ts = self.func_tscope(f)?;
        let fd = &self.funcs[f as usize];
        if fd.kind == FnKind::Ctor {
            self.funcs[f as usize].ret = Some(Type::Void);
            return Ok(Type::Void);
        }
        if self.erase {
            self.funcs[f as usize].ret = Some(Type::Any);
            return Ok(Type::Any);
        }
        if let Some(a) = fd.ast.clone()
            && let Some(rt) = &a.ret
        {
            let t = self.resolve_type_in(rt, &ts)?;
            self.funcs[f as usize].ret = Some(t.clone());
            return Ok(t);
        }
        // Inferred: check the body now.
        match self.funcs[f as usize].state {
            State::Checking => {
                let fd = &self.funcs[f as usize];
                return err(
                    format!(
                        "'{}' needs an explicit return type annotation because it is used recursively",
                        fd.name
                    ),
                    fd.span,
                );
            }
            _ => self.ensure_checked(f)?,
        }
        Ok(self.funcs[f as usize].ret.clone().unwrap_or(Type::Void))
    }

    pub fn arrival_kinds(&mut self, f: FuncId) -> CResult<Vec<Kind>> {
        let ps = self.func_params(f)?;
        Ok(ps.iter().map(param_arrival).collect())
    }

    /// Check a function body (once). Produces its HIR.
    pub fn ensure_checked(&mut self, f: FuncId) -> CResult<()> {
        if self.funcs[f as usize].state != State::Unchecked {
            return Ok(());
        }
        // Class members are checked via their class.
        if let Some(c) = self.funcs[f as usize].class {
            self.ensure_class(c)?;
        }
        if self.funcs[f as usize].state != State::Unchecked {
            return Ok(());
        }
        self.funcs[f as usize].state = State::Checking;
        let r = self.check_func_body(f);
        self.funcs[f as usize].state = State::Done;
        r
    }

    fn check_func_body(&mut self, f: FuncId) -> CResult<()> {
        let params = self.func_params(f)?;
        let ts = self.func_tscope(f)?;
        let fd = &self.funcs[f as usize];
        let kind = fd.kind;
        let class = fd.class;
        let this_ty = fd.this_ty.clone();
        let ast = fd.ast.clone();
        let declared_ret = match &ast {
            Some(a) if a.ret.is_some() => Some(self.func_ret(f)?),
            _ if kind == FnKind::Ctor => Some(Type::Void),
            _ => None,
        };
        if let Some(a) = &ast {
            if a.is_async {
                return err("async functions are not supported yet", a.span);
            }
            if a.is_generator {
                return err("generators are not supported yet", a.span);
            }
        }
        let fscope = Scope::new(Some(ts), Some(f));
        let this_var = match kind {
            FnKind::Arrow | FnKind::Normal => None,
            _ => {
                let v = self.new_var("this".into(), this_ty.clone(), true, &fscope);
                fscope.decls.borrow_mut().pop();
                Some(v)
            }
        };
        let inherited = if kind == FnKind::Arrow || kind == FnKind::Normal {
            self.pending_narrow.take().unwrap_or_default()
        } else {
            self.pending_narrow = None;
            Narrow::default()
        };
        let saved_param_stmts = std::mem::take(&mut self.param_stmts);
        self.stack.push(FnCtx {
            func: f,
            scope: fscope.clone(),
            fn_scope: fscope.clone(),
            ret: declared_ret.clone(),
            returns: vec![],
            narrow: inherited,
            this_var,
            this_ty: this_ty.clone(),
            kind,
            class,
            ctor_inits: None,
        });
        let r = self.check_func_inner(f, &params, &fscope, ast.as_deref());
        let ctx = self.stack.pop().unwrap();
        self.param_stmts = saved_param_stmts;
        let (hparams, body) = r?;
        let ret = match declared_ret {
            Some(t) => t,
            None => {
                let falls_off = match ast.as_ref().and_then(|a| a.body.as_ref()) {
                    Some(ast::Body::Block(b)) => !stmt::block_terminates(b),
                    _ => false,
                };
                let mut rs = ctx.returns.clone();
                if rs.is_empty() {
                    Type::Void
                } else {
                    if falls_off {
                        rs.push(Type::Undefined);
                    }
                    Type::union(rs.iter().map(|t| t.widen())).widen()
                }
            }
        };
        let ret_kind = match ret {
            Type::Void | Type::Undefined | Type::Never => None,
            ref t => Some(t.kind()),
        };
        self.funcs[f as usize].ret = Some(ret);
        let name = self.funcs[f as usize].name.clone();
        self.funcs[f as usize].hir = Some(HFunc {
            name,
            this_var,
            params: hparams,
            ret_kind,
            body,
            free: vec![],
            parent: self.funcs[f as usize].parent,
            is_ctor: kind == FnKind::Ctor,
            class,
        });
        Ok(())
    }

    fn check_func_inner(
        &mut self,
        f: FuncId,
        params: &[FParam],
        fscope: &Rc<Scope>,
        ast: Option<&ast::Function>,
    ) -> CResult<(Vec<HParam>, Vec<HStmt>)> {
        let mut hparams = Vec::new();
        let mut prologue = Vec::new();
        if let Some(a) = ast {
            for (p, fp) in a.params.iter().zip(params.iter()) {
                let var_ty = fp.ty.clone();
                let arrival = param_arrival(fp);
                match &p.pat {
                    ast::Pattern::Ident(n, _) => {
                        let v = self.new_var(n.clone(), Some(var_ty.clone()), false, fscope);
                        fscope.decls.borrow_mut().pop();
                        fscope.vals.borrow_mut().insert(n.clone(), VB::Var(v));
                        let default = match &p.default {
                            Some(d) => {
                                let e = self.check_expr(d, Some(&var_ty))?;
                                self.expect_assignable(&e.ty, &var_ty, d.span)?;
                                Some(e)
                            }
                            None => None,
                        };
                        hparams.push(HParam {
                            var: v,
                            arrival,
                            default,
                            rest: p.rest,
                        });
                        if p.property {
                            prologue.push((n.clone(), v));
                        }
                    }
                    pat => {
                        // Destructured parameter: bind a hidden variable, destructure in the body.
                        let v = self.new_var("$param".into(), Some(var_ty.clone()), false, fscope);
                        fscope.decls.borrow_mut().pop();
                        let default = match &p.default {
                            Some(d) => Some(self.check_expr(d, Some(&var_ty))?),
                            None => None,
                        };
                        hparams.push(HParam {
                            var: v,
                            arrival,
                            default,
                            rest: p.rest,
                        });
                        let src = HExpr::read(Place::Var(v), var_ty.clone());
                        let stmts = self.bind_pattern(pat, src, ast::VarKind::Let, None)?;
                        self.pending_param_stmts(stmts);
                    }
                }
            }
        }
        // Constructor: parameter properties and field initializers.
        let kind = self.funcs[f as usize].kind;
        if kind == FnKind::Ctor {
            let cid = self.funcs[f as usize].class.unwrap();
            let mut inits = Vec::new();
            let this_v = self.ctx().this_var.unwrap();
            let this_ty = self.ctx().this_ty.clone().unwrap();
            for (n, v) in prologue {
                let slot = self.classes[cid as usize].fields.iter().position(|fd| fd.name == n).unwrap();
                let ty = self.vars[v as usize].ty.clone();
                inits.push(HStmt::Expr(HExpr::new(
                    HK::Assign(
                        Box::new(Place::Field {
                            obj: HExpr::read(Place::Var(this_v), this_ty.clone()),
                            slot: slot as u32,
                            opt: false,
                        }),
                        Box::new(HExpr::read(Place::Var(v), ty.clone())),
                    ),
                    ty,
                )));
            }
            let own_start = self.classes[cid as usize].own_field_start;
            let fields = self.classes[cid as usize].fields.clone();
            for (slot, fdef) in fields.iter().enumerate().skip(own_start) {
                let Some(init) = &fdef.init else { continue };
                let fty = fdef.ty.clone().unwrap_or(Type::Any);
                let e = self.check_expr(init, Some(&fty))?;
                self.expect_assignable(&e.ty, &fty, init.span)?;
                inits.push(HStmt::Expr(HExpr::new(
                    HK::Assign(
                        Box::new(Place::Field {
                            obj: HExpr::read(Place::Var(this_v), this_ty.clone()),
                            slot: slot as u32,
                            opt: false,
                        }),
                        Box::new(e.cast(fty.clone())),
                    ),
                    fty,
                )));
            }
            self.ctx_mut().ctor_inits = Some(inits);
        }
        let mut body = std::mem::take(&mut self.param_stmts);
        let has_parent = self.funcs[f as usize]
            .class
            .is_some_and(|c| self.classes[c as usize].parent.is_some());
        match ast.and_then(|a| a.body.as_ref()) {
            Some(ast::Body::Block(stmts)) => {
                if kind == FnKind::Ctor && !has_parent {
                    body.extend(self.ctx_mut().ctor_inits.take().unwrap_or_default());
                }
                let scope = self.ctx().scope.clone();
                let b = self.check_block_in(stmts, scope)?;
                body.extend(b);
                if kind == FnKind::Ctor
                    && let Some(rest) = self.ctx_mut().ctor_inits.take()
                    && !rest.is_empty()
                {
                    // No explicit super() call found at the top level.
                    body.splice(0..0, rest);
                }
            }
            Some(ast::Body::Expr(e)) => {
                let want = self.ctx().ret.clone();
                let he = self.check_expr(e, want.as_ref())?;
                if let Some(w) = &want {
                    self.expect_assignable(&he.ty, w, e.span)?;
                }
                self.ctx_mut().returns.push(he.ty.clone());
                let he = match &want {
                    Some(w) => he.cast(w.clone()),
                    None => he,
                };
                body.push(HStmt::Return(Some(he)));
            }
            None => {
                if kind == FnKind::Ctor {
                    // Implicit constructor.
                    let cid = self.funcs[f as usize].class.unwrap();
                    if let Some((p, _)) = self.classes[cid as usize].parent.clone() {
                        let pctor = self.classes[p as usize].ctor;
                        let this_v = self.ctx().this_var.unwrap();
                        let this_ty = self.ctx().this_ty.clone().unwrap();
                        // Forward all arguments: implicit ctor takes the parent's params.
                        let pparams = self.func_params(pctor)?;
                        let mut args = Vec::new();
                        for (i, pp) in pparams.iter().enumerate() {
                            let v = hparams[i].var;
                            let _ = pp;
                            args.push(HElem::Expr(HExpr::read(Place::Var(v), self.vars[v as usize].ty.clone())));
                        }
                        let kinds = self.arrival_kinds(pctor)?;
                        body.push(HStmt::Expr(HExpr::new(
                            HK::Call(Box::new(HCall {
                                callee: Callee::Direct { func: pctor, closure: None },
                                this: Some(HExpr::read(Place::Var(this_v), this_ty)),
                                args,
                                param_kinds: Some(kinds),
                                rest_at: None,
                                ret_kind: Kind::Val,
                                opt: false,
                            })),
                            Type::Void,
                        )));
                    }
                    body.extend(self.ctx_mut().ctor_inits.take().unwrap_or_default());
                } else {
                    // Abstract method / overload signature: throws if ever called.
                    body.push(HStmt::Throw(HExpr::str("abstract method called")));
                }
            }
        }
        let vars = fscope.decls.borrow().clone();
        Ok((hparams, vec![HStmt::Block(body, vars)]))
    }

    fn pending_param_stmts(&mut self, stmts: Vec<HStmt>) {
        self.param_stmts.extend(stmts);
    }

    pub fn expect_assignable(&mut self, src: &Type, dst: &Type, span: Span) -> CResult<()> {
        if self.assignable(src, dst) {
            Ok(())
        } else {
            err(
                format!("type '{}' is not assignable to type '{}'", self.show(src), self.show(dst)),
                span,
            )
        }
    }

    // ───────────────────────────── classes ─────────────────────────────

    /// Resolve a class's members (fields, methods, vtable, constructor).
    pub fn ensure_class(&mut self, cid: ClassId) -> CResult<()> {
        if self.classes[cid as usize].resolved {
            return Ok(());
        }
        self.classes[cid as usize].resolved = true;
        let decl = self.classes[cid as usize].decl.clone();
        let cscope = self.classes[cid as usize].cscope.clone();
        let tparams = self.classes[cid as usize].tparams.clone();
        let this_ty = Type::Class(cid, tparams.iter().map(|t| Type::Param(*t)).collect());
        let ctor_this = this_ty.clone();
        let saved_scope = self.ctx().scope.clone();
        self.ctx_mut().scope = cscope.clone();
        let r = (|| -> CResult<()> {
            // Parent.
            let mut fields: Vec<FieldDef> = Vec::new();
            let mut vtable: Vec<(Name, FuncId)> = Vec::new();
            if let Some((pe, pargs)) = &decl.extends {
                let pid = match &pe.kind {
                    ast::ExprKind::Ident(n) => match self.lookup_val(n) {
                        Some(VB::Class(p)) => p,
                        _ => return err(format!("'{n}' is not a class"), pe.span),
                    },
                    _ => return err("only class names are supported after 'extends'", pe.span),
                };
                if pid == cid {
                    return err("a class cannot extend itself", pe.span);
                }
                self.ensure_class(pid)?;
                let mut pa = Vec::new();
                for a in pargs {
                    pa.push(self.resolve_type(a)?);
                }
                pa.resize(self.classes[pid as usize].tparams.len(), Type::Any);
                let map = self.class_subst(pid, &pa);
                for fd in &self.classes[pid as usize].fields {
                    let mut fd = fd.clone();
                    fd.ty = fd.ty.map(|t| self.subst(&t, &map));
                    fd.init = None;
                    fields.push(fd);
                }
                vtable = self.classes[pid as usize].vtable.clone();
                self.classes[cid as usize].parent = Some((pid, pa));
            }
            let own_start = fields.len();
            let mut methods: FxHashMap<Name, (FuncId, u32)> = FxHashMap::default();
            for (i, (n, f)) in vtable.iter().enumerate() {
                methods.insert(n.clone(), (*f, i as u32));
            }
            let mut ctor_ast = None;
            let mut accessors = match &self.classes[cid as usize].parent {
                Some((p, _)) => self.classes[*p as usize].accessors.clone(),
                None => FxHashMap::default(),
            };
            // Parameter properties come first in the layout (as Node's TS transform does).
            for m in &decl.members {
                if let ast::ClassMember::Constructor(f) = m {
                    for p in &f.params {
                        if p.property
                            && let ast::Pattern::Ident(n, _) = &p.pat
                        {
                            let mut t = match &p.ty {
                                Some(t) => self.resolve_type(t)?,
                                None => Type::Any,
                            };
                            if p.optional {
                                t = Type::union([t, Type::Undefined]);
                            }
                            if !fields.iter().any(|fd| fd.name == *n) {
                                fields.push(FieldDef {
                                    name: n.clone(),
                                    ty: Some(if self.erase { Type::Any } else { t }),
                                    ty_expr: p.ty.clone(),
                                    init: None,
                                    optional: p.optional,
                                });
                            }
                        }
                    }
                }
            }
            let mut statics: FxHashMap<Name, VB> = FxHashMap::default();
            let mut static_inits: Vec<(VarId, ast::Expr)> = Vec::new();
            for m in &decl.members {
                match m {
                    ast::ClassMember::Field {
                        name,
                        ty,
                        init,
                        is_static,
                        optional,
                        span,
                    } => {
                        let mut t = match ty {
                            Some(t) => Some(self.resolve_type(t)?),
                            None => None,
                        };
                        if *optional {
                            t = Some(Type::union([t.unwrap_or(Type::Any), Type::Undefined]));
                        }
                        if *is_static {
                            let t = match (t, init) {
                                (Some(t), _) => t,
                                (None, Some(e)) => {
                                    let he = self.check_expr(e, None)?;
                                    he.ty.widen()
                                }
                                (None, None) => Type::Any,
                            };
                            let v = self.new_var(name.clone(), Some(t), false, &self.classes[cid as usize].scope.clone());
                            statics.insert(name.clone(), VB::Var(v));
                            if let Some(e) = init {
                                static_inits.push((v, e.clone()));
                            }
                            continue;
                        }
                        let t = match (t, init) {
                            (Some(t), _) => t,
                            (None, Some(e)) => {
                                // Infer from the initializer (checked again inside the constructor).
                                match self.literal_type(e) {
                                    Some(t) => t,
                                    None => {
                                        self.stack.push(FnCtx {
                                            func: self.ctx().func,
                                            scope: cscope.clone(),
                                            fn_scope: cscope.clone(),
                                            ret: None,
                                            returns: vec![],
                                            narrow: Narrow::default(),
                                            this_var: None,
                                            this_ty: None,
                                            kind: FnKind::Normal,
                                            class: Some(cid),
                                            ctor_inits: None,
                                        });
                                        let r = self.check_expr(e, None);
                                        self.stack.pop();
                                        r?.ty.widen()
                                    }
                                }
                            }
                            (None, None) => Type::Any,
                        };
                        let fd = FieldDef {
                            name: name.clone(),
                            ty: Some(if self.erase { Type::Any } else { t }),
                            ty_expr: ty.clone(),
                            init: init.clone(),
                            optional: *optional,
                        };
                        match fields.iter().position(|f| f.name == *name) {
                            Some(i) => fields[i] = fd,
                            None => fields.push(fd),
                        }
                        let _ = span;
                    }
                    ast::ClassMember::Method {
                        name,
                        func,
                        is_static,
                        kind,
                    } => {
                        if *kind != ast::MethodKind::Method {
                            if *is_static {
                                return err("static accessors are not supported yet", func.span);
                            }
                            let vname: Name = format!("{} {name}", if *kind == ast::MethodKind::Get { "get" } else { "set" }).into();
                            let f = self.new_func(
                                vname.clone(),
                                Some(func.clone()),
                                FnKind::Method,
                                Some(cid),
                                None,
                                cscope.clone(),
                                Some(this_ty.clone()),
                            );
                            let slot = match vtable.iter().position(|(n, _)| *n == vname) {
                                Some(i) => {
                                    vtable[i].1 = f;
                                    i as u32
                                }
                                None => {
                                    vtable.push((vname.clone(), f));
                                    vtable.len() as u32 - 1
                                }
                            };
                            methods.insert(vname, (f, slot));
                            let e = accessors.entry(name.clone()).or_insert((None, None));
                            if *kind == ast::MethodKind::Get {
                                e.0 = Some((f, slot));
                            } else {
                                e.1 = Some((f, slot));
                            }
                            continue;
                        }
                        if func.body.is_none() && !decl.is_abstract && !decl.members.iter().any(|m| matches!(m, ast::ClassMember::Method { name: n2, func: f2, .. } if n2 == name && f2.body.is_some())) {
                            return err(format!("method '{name}' has no body"), func.span);
                        }
                        if func.body.is_none() && methods.contains_key(name) && !decl.is_abstract {
                            continue; // overload signature
                        }
                        if *is_static {
                            let f = self.new_func(
                                format!("{}.{}", decl.name.as_deref().unwrap_or("<class>"), name).into(),
                                Some(func.clone()),
                                FnKind::StaticMethod,
                                Some(cid),
                                None,
                                cscope.clone(),
                                Some(Type::ClassCtor(cid)),
                            );
                            let v = self.new_var(name.clone(), None, true, &self.classes[cid as usize].scope.clone());
                            statics.insert(name.clone(), VB::Func(v, f));
                            continue;
                        }
                        // Overloads: keep only the implementation (the one with a body).
                        if func.body.is_none() && decl.members.iter().any(|m| matches!(m, ast::ClassMember::Method { name: n2, func: f2, .. } if n2 == name && f2.body.is_some())) {
                            continue;
                        }
                        let f = self.new_func(
                            name.clone(),
                            Some(func.clone()),
                            FnKind::Method,
                            Some(cid),
                            None,
                            cscope.clone(),
                            Some(this_ty.clone()),
                        );
                        match vtable.iter().position(|(n, _)| n == name) {
                            Some(i) => {
                                vtable[i].1 = f;
                                methods.insert(name.clone(), (f, i as u32));
                            }
                            None => {
                                vtable.push((name.clone(), f));
                                methods.insert(name.clone(), (f, vtable.len() as u32 - 1));
                            }
                        }
                    }
                    ast::ClassMember::Constructor(f) => {
                        if f.body.is_some() || ctor_ast.is_none() {
                            ctor_ast = Some(f.clone());
                        }
                        // Parameter properties become fields.
                        for p in &f.params {
                            if p.property
                                && let ast::Pattern::Ident(n, _) = &p.pat
                            {
                                let mut t = match &p.ty {
                                    Some(t) => self.resolve_type(t)?,
                                    None => Type::Any,
                                };
                                if p.optional {
                                    t = Type::union([t, Type::Undefined]);
                                }
                                if !fields.iter().any(|fd| fd.name == *n) {
                                    fields.push(FieldDef {
                                        name: n.clone(),
                                        ty: Some(if self.erase { Type::Any } else { t }),
                                        ty_expr: p.ty.clone(),
                                        init: None,
                                        optional: p.optional,
                                    });
                                }
                            }
                        }
                    }
                    ast::ClassMember::StaticBlock(_) => {
                        return err("static blocks are not supported yet", decl.span);
                    }
                }
            }
            // Constructor function.
            let cname: Name = decl.name.clone().unwrap_or_else(|| "<class>".into());
            let ctor_fn_ast = match ctor_ast {
                Some(f) => Some(f),
                None => {
                    // Implicit constructor: same parameters as the parent's.
                    match &self.classes[cid as usize].parent {
                        Some((p, _)) => {
                            let pc = self.classes[*p as usize].ctor;
                            self.funcs[pc as usize].ast.clone().map(|a| {
                                let mut a = (*a).clone();
                                a.body = None;
                                for p in &mut a.params {
                                    p.property = false;
                                }
                                Rc::new(a)
                            })
                        }
                        None => None,
                    }
                }
            };
            let ctor = self.new_func(cname.clone(), ctor_fn_ast, FnKind::Ctor, Some(cid), None, cscope.clone(), Some(ctor_this.clone()));
            let c = &mut self.classes[cid as usize];
            c.fields = fields;
            c.own_field_start = own_start;
            c.methods = methods;
            c.vtable = vtable;
            c.ctor = ctor;
            c.statics = statics;
            c.accessors = accessors;
            // Static initializers run where the class is declared.
            self.static_inits.extend(static_inits.into_iter().map(|(v, e)| (cid, v, e)));
            Ok(())
        })();
        self.ctx_mut().scope = saved_scope;
        r
    }

    // ───────────────────────────── output ─────────────────────────────

    fn finish(mut self) -> CResult<HProgram> {
        let mut funcs = Vec::new();
        for (i, fd) in self.funcs.iter_mut().enumerate() {
            let mut h = fd.hir.take().unwrap_or_else(|| panic!("function {} ({}) was never checked", i, fd.name));
            h.free = fd.free.clone();
            if fd.class.is_some() && !fd.free.is_empty() && fd.kind != FnKind::Arrow {
                return err(
                    format!(
                        "class methods that capture local variables are not supported yet ('{}'); declare the class at top level",
                        fd.name
                    ),
                    fd.span,
                );
            }
            funcs.push(h);
        }
        let mut classes = Vec::new();
        let mut error_class = None;
        let mut type_error_class = None;
        let mut range_error_class = None;
        for (i, c) in self.classes.iter().enumerate() {
            if c.prelude_error {
                match &*c.name {
                    "Error" => error_class = Some(i as u32),
                    "TypeError" => type_error_class = Some(i as u32),
                    "RangeError" => range_error_class = Some(i as u32),
                    _ => {}
                }
            }
            classes.push(HClass {
                name: c.name.clone(),
                parent: c.parent.as_ref().map(|p| p.0),
                fields: c.fields.iter().map(|f| f.name.clone()).collect(),
                vtable: c.vtable.clone(),
                ctor: c.ctor,
            });
        }
        Ok(HProgram {
            funcs,
            vars: self.vars,
            classes,
            main: MAIN,
            error_class,
            type_error_class,
            range_error_class,
        })
    }
}

pub fn pat_name(p: &ast::Pattern) -> Name {
    match p {
        ast::Pattern::Ident(n, _) => n.clone(),
        _ => "$destructured".into(),
    }
}

pub fn param_arrival(p: &FParam) -> Kind {
    if p.optional || p.rest { Kind::Val } else { p.ty.kind() }
}

#[allow(dead_code)]
fn unused(_: FxHashSet<u32>) {}
