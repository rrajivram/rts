//! Untyped syntax tree produced by the parser.

use std::rc::Rc;

use crate::error::Span;

pub type Name = Rc<str>;

// ───────────────────────────── type annotations ─────────────────────────────

#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// `number`, `Foo`, `Array<T>`, `T`, `Ns.Foo` (dotted names joined).
    Ref {
        name: Name,
        args: Vec<TypeExpr>,
        span: Span,
    },
    Array(Box<TypeExpr>),
    Tuple(Vec<TypeExpr>),
    Union(Vec<TypeExpr>),
    Intersection(Vec<TypeExpr>),
    Func {
        tparams: Vec<TypeParam>,
        params: Vec<Param>,
        ret: Box<TypeExpr>,
    },
    Object(Vec<TypeMember>),
    StrLit(Name),
    NumLit(f64),
    BoolLit(bool),
    /// `typeof x` in type position.
    TypeOf(Name, Span),
    /// `keyof T`
    KeyOf(Box<TypeExpr>),
    /// `T[K]`
    Indexed(Box<TypeExpr>, Box<TypeExpr>),
    /// `{ [K in Keys as N]?: V }` (`optional`/`readonly`: Some(true) adds, Some(false) removes)
    Mapped {
        param: Name,
        keys: Box<TypeExpr>,
        rename: Option<Box<TypeExpr>>,
        value: Box<TypeExpr>,
        optional: Option<bool>,
    },
    /// `C extends E ? T : F`
    Conditional {
        check: Box<TypeExpr>,
        extends: Box<TypeExpr>,
        then: Box<TypeExpr>,
        otherwise: Box<TypeExpr>,
    },
    /// `infer U` (inside a conditional type's `extends`)
    Infer(Name),
    /// A template literal type: `` `id-${number}` ``
    TemplateLit,
}

#[derive(Debug, Clone)]
pub enum TypeMember {
    Prop {
        name: Name,
        optional: bool,
        readonly: bool,
        ty: TypeExpr,
    },
    /// `[key: string]: T`
    Index { key: TypeExpr, value: TypeExpr },
    /// Call signature `(x: number): string`.
    Call(TypeExpr),
}

#[derive(Debug, Clone)]
pub struct TypeParam {
    pub name: Name,
    pub constraint: Option<TypeExpr>,
    pub default: Option<TypeExpr>,
}

// ───────────────────────────── expressions ─────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    InstanceOf,
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Plus,
    Not,
    BitNot,
    TypeOf,
    Void,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Bin(BinOp),
    Logical(LogicalOp),
}

#[derive(Debug, Clone)]
pub enum ArrayElem {
    Expr(Expr),
    Spread(Expr),
    Hole,
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Name(Name),
    Computed(Expr),
}

#[derive(Debug, Clone)]
pub enum ObjProp {
    /// `key: value`, shorthand `key` (value = Ident), and methods (value = Func).
    Init { key: PropKey, value: Expr },
    Spread(Expr),
    /// `get key() {}` (is_get) or `set key(v) {}`
    Accessor { key: Name, is_get: bool, func: Rc<Function> },
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    /// `123n`
    BigInt(i128),
    Str(Name),
    /// `/pattern/flags`
    Regex(Name, Name),
    Template {
        quasis: Vec<Name>,
        exprs: Vec<Expr>,
    },
    Bool(bool),
    Null,
    This,
    Super,
    Ident(Name),
    Array(Vec<ArrayElem>),
    Object(Vec<ObjProp>),
    Func(Rc<Function>),
    Class(Rc<ClassDecl>),
    Unary(UnaryOp, Box<Expr>),
    Update {
        inc: bool,
        prefix: bool,
        target: Box<Expr>,
    },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Assign(AssignOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call {
        callee: Box<Expr>,
        targs: Vec<TypeExpr>,
        args: Vec<ArrayElem>,
        optional: bool,
    },
    New {
        callee: Box<Expr>,
        targs: Vec<TypeExpr>,
        args: Vec<ArrayElem>,
    },
    Member {
        obj: Box<Expr>,
        prop: Name,
        optional: bool,
    },
    Index {
        obj: Box<Expr>,
        index: Box<Expr>,
        optional: bool,
    },
    /// The boundary of an optional chain: `a?.b.c` short-circuits up to here.
    OptChain(Box<Expr>),
    As(Box<Expr>, TypeExpr),
    /// `x satisfies T`
    Satisfies(Box<Expr>, TypeExpr),
    NonNull(Box<Expr>),
    Seq(Vec<Expr>),
    Paren(Box<Expr>),
    Await(Box<Expr>),
    /// `yield x`, `yield`, `yield* x` (delegate).
    Yield {
        arg: Option<Box<Expr>>,
        delegate: bool,
    },
}

// ───────────────────────────── functions & classes ─────────────────────────────

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(Name, Span),
    /// `{ a, b: c, d = 1, ...rest }`
    Object {
        props: Vec<(Name, Pattern, Option<Expr>)>,
        rest: Option<Box<Pattern>>,
        span: Span,
    },
    /// `[a, , b = 2, ...rest]`
    Array {
        elems: Vec<Option<(Pattern, Option<Expr>)>>,
        rest: Option<Box<Pattern>>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Ident(_, s) | Pattern::Object { span: s, .. } | Pattern::Array { span: s, .. } => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pat: Pattern,
    pub ty: Option<TypeExpr>,
    pub optional: bool,
    pub default: Option<Expr>,
    pub rest: bool,
    /// Constructor parameter property (`constructor(private x: number)`).
    pub property: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Body {
    Block(Vec<Stmt>),
    /// Arrow function with an expression body.
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: Option<Name>,
    pub tparams: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Option<Body>,
    pub is_arrow: bool,
    pub is_async: bool,
    pub is_generator: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Method,
    Get,
    Set,
}

#[derive(Debug, Clone)]
pub enum ClassMember {
    Field {
        name: Name,
        ty: Option<TypeExpr>,
        init: Option<Expr>,
        is_static: bool,
        optional: bool,
        span: Span,
    },
    Method {
        name: Name,
        func: Rc<Function>,
        is_static: bool,
        kind: MethodKind,
    },
    Constructor(Rc<Function>),
    /// `static { ... }`
    StaticBlock(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: Option<Name>,
    pub tparams: Vec<TypeParam>,
    pub extends: Option<(Expr, Vec<TypeExpr>)>,
    pub implements: Vec<TypeExpr>,
    pub members: Vec<ClassMember>,
    pub is_abstract: bool,
    pub span: Span,
}

// ───────────────────────────── statements ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone)]
pub struct VarDeclarator {
    pub pat: Pattern,
    pub ty: Option<TypeExpr>,
    pub init: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Var {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
        declare: bool,
    },
    Func(Rc<Function>),
    Class(Rc<ClassDecl>),
    Interface {
        name: Name,
        tparams: Vec<TypeParam>,
        extends: Vec<TypeExpr>,
        members: Vec<TypeMember>,
    },
    TypeAlias {
        name: Name,
        tparams: Vec<TypeParam>,
        ty: TypeExpr,
    },
    Enum {
        name: Name,
        members: Vec<(Name, Option<Expr>, Span)>,
    },
    Expr(Expr),
    Block(Vec<Stmt>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For {
        init: Option<Box<Stmt>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    ForOf {
        kind: VarKind,
        pat: Pattern,
        iter: Expr,
        body: Box<Stmt>,
        /// `for await`
        is_await: bool,
    },
    ForIn {
        kind: VarKind,
        name: Name,
        obj: Expr,
        body: Box<Stmt>,
    },
    Break(Option<Name>),
    Continue(Option<Name>),
    Return(Option<Expr>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        param: Option<Pattern>,
        handler: Option<Vec<Stmt>>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch(Expr, Vec<Case>),
    Labeled(Name, Box<Stmt>),
    Empty,
    Import(Rc<ImportDecl>),
    Export(Rc<ExportDecl>),
    /// `namespace X { … }` (becomes a module of its own; see `module.rs`).
    Namespace { name: Name, body: Vec<Stmt> },
}

// ───────────────────────────── modules ─────────────────────────────

/// `import d, * as ns from "m"`, `import { a, b as c } from "m"`, `import "m"`.
#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub default: Option<Name>,
    pub namespace: Option<Name>,
    pub named: Vec<ImportSpec>,
    pub from: Name,
    /// `import type …`: erased.
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub imported: Name,
    pub local: Name,
    pub type_only: bool,
}

#[derive(Debug, Clone)]
pub enum ExportDecl {
    /// `export const/let/function/class/interface/type/enum …`
    Decl(Stmt),
    /// `export default function …` / `export default class …` (named or not).
    DefaultDecl(Stmt),
    /// `export default <expr>`
    DefaultExpr(Expr),
    /// `export { a, b as c }` or `export { a } from "m"`: (local, exported).
    Named {
        specs: Vec<(Name, Name)>,
        from: Option<Name>,
        span: Span,
    },
    /// `export * from "m"` / `export * as ns from "m"`
    All { from: Name, alias: Option<Name>, span: Span },
}

// ───────────────────────────── traversal ─────────────────────────────

/// Visit every expression in `stmts` (including inside nested functions and
/// classes), parents before children. `f` may replace the expression; the
/// replacement's children are visited next.
pub fn visit_exprs_mut(stmts: &mut [Stmt], f: &mut dyn FnMut(&mut Expr)) {
    for s in stmts {
        visit_stmt(s, f);
    }
}

fn visit_stmt(s: &mut Stmt, f: &mut dyn FnMut(&mut Expr)) {
    match &mut s.kind {
        StmtKind::Var { decls, .. } => {
            for d in decls {
                visit_pattern(&mut d.pat, f);
                if let Some(e) = &mut d.init {
                    visit_expr(e, f);
                }
            }
        }
        StmtKind::Func(func) => visit_function(Rc::make_mut(func), f),
        StmtKind::Class(c) => visit_class(Rc::make_mut(c), f),
        StmtKind::Expr(e) | StmtKind::Throw(e) => visit_expr(e, f),
        StmtKind::Return(e) => {
            if let Some(e) = e {
                visit_expr(e, f);
            }
        }
        StmtKind::Block(b) => visit_exprs_mut(b, f),
        StmtKind::If(c, t, e) => {
            visit_expr(c, f);
            visit_stmt(t, f);
            if let Some(e) = e {
                visit_stmt(e, f);
            }
        }
        StmtKind::While(c, b) => {
            visit_expr(c, f);
            visit_stmt(b, f);
        }
        StmtKind::DoWhile(b, c) => {
            visit_stmt(b, f);
            visit_expr(c, f);
        }
        StmtKind::For { init, test, update, body } => {
            if let Some(i) = init {
                visit_stmt(i, f);
            }
            if let Some(t) = test {
                visit_expr(t, f);
            }
            if let Some(u) = update {
                visit_expr(u, f);
            }
            visit_stmt(body, f);
        }
        StmtKind::ForOf { pat, iter, body, .. } => {
            visit_pattern(pat, f);
            visit_expr(iter, f);
            visit_stmt(body, f);
        }
        StmtKind::ForIn { obj, body, .. } => {
            visit_expr(obj, f);
            visit_stmt(body, f);
        }
        StmtKind::Try { block, handler, finalizer, param } => {
            visit_exprs_mut(block, f);
            if let Some(p) = param {
                visit_pattern(p, f);
            }
            if let Some(h) = handler {
                visit_exprs_mut(h, f);
            }
            if let Some(fin) = finalizer {
                visit_exprs_mut(fin, f);
            }
        }
        StmtKind::Switch(d, cases) => {
            visit_expr(d, f);
            for c in cases {
                if let Some(t) = &mut c.test {
                    visit_expr(t, f);
                }
                visit_exprs_mut(&mut c.body, f);
            }
        }
        StmtKind::Labeled(_, b) => visit_stmt(b, f),
        StmtKind::Export(e) => match Rc::make_mut(e) {
            ExportDecl::Decl(s) | ExportDecl::DefaultDecl(s) => visit_stmt(s, f),
            ExportDecl::DefaultExpr(e) => visit_expr(e, f),
            _ => {}
        },
        StmtKind::Interface { .. } | StmtKind::TypeAlias { .. } | StmtKind::Enum { .. } => {
            if let StmtKind::Enum { members, .. } = &mut s.kind {
                for (_, init, _) in members {
                    if let Some(e) = init {
                        visit_expr(e, f);
                    }
                }
            }
        }
        StmtKind::Namespace { body, .. } => visit_exprs_mut(body, f),
        StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Empty | StmtKind::Import(_) => {}
    }
}

fn visit_pattern(p: &mut Pattern, f: &mut dyn FnMut(&mut Expr)) {
    match p {
        Pattern::Ident(..) => {}
        Pattern::Object { props, rest, .. } => {
            for (_, p, d) in props {
                visit_pattern(p, f);
                if let Some(d) = d {
                    visit_expr(d, f);
                }
            }
            if let Some(r) = rest {
                visit_pattern(r, f);
            }
        }
        Pattern::Array { elems, rest, .. } => {
            for (p, d) in elems.iter_mut().flatten() {
                visit_pattern(p, f);
                if let Some(d) = d {
                    visit_expr(d, f);
                }
            }
            if let Some(r) = rest {
                visit_pattern(r, f);
            }
        }
    }
}

fn visit_function(func: &mut Function, f: &mut dyn FnMut(&mut Expr)) {
    for p in &mut func.params {
        visit_pattern(&mut p.pat, f);
        if let Some(d) = &mut p.default {
            visit_expr(d, f);
        }
    }
    match &mut func.body {
        Some(Body::Block(b)) => visit_exprs_mut(b, f),
        Some(Body::Expr(e)) => visit_expr(e, f),
        None => {}
    }
}

fn visit_class(c: &mut ClassDecl, f: &mut dyn FnMut(&mut Expr)) {
    if let Some((e, _)) = &mut c.extends {
        visit_expr(e, f);
    }
    for m in &mut c.members {
        match m {
            ClassMember::Field { init, .. } => {
                if let Some(e) = init {
                    visit_expr(e, f);
                }
            }
            ClassMember::Method { func, .. } | ClassMember::Constructor(func) => visit_function(Rc::make_mut(func), f),
            ClassMember::StaticBlock(b) => visit_exprs_mut(b, f),
        }
    }
}

fn visit_elems(elems: &mut [ArrayElem], f: &mut dyn FnMut(&mut Expr)) {
    for e in elems {
        match e {
            ArrayElem::Expr(e) | ArrayElem::Spread(e) => visit_expr(e, f),
            ArrayElem::Hole => {}
        }
    }
}

pub fn visit_expr(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    f(e);
    match &mut e.kind {
        ExprKind::Template { exprs, .. } => {
            for x in exprs {
                visit_expr(x, f);
            }
        }
        ExprKind::Array(elems) => visit_elems(elems, f),
        ExprKind::Object(props) => {
            for p in props {
                match p {
                    ObjProp::Init { key, value } => {
                        if let PropKey::Computed(k) = key {
                            visit_expr(k, f);
                        }
                        visit_expr(value, f);
                    }
                    ObjProp::Spread(x) => visit_expr(x, f),
                    ObjProp::Accessor { func, .. } => visit_function(Rc::make_mut(func), f),
                }
            }
        }
        ExprKind::Func(func) => visit_function(Rc::make_mut(func), f),
        ExprKind::Class(c) => visit_class(Rc::make_mut(c), f),
        ExprKind::Unary(_, x)
        | ExprKind::Update { target: x, .. }
        | ExprKind::OptChain(x)
        | ExprKind::As(x, _)
        | ExprKind::Satisfies(x, _)
        | ExprKind::NonNull(x)
        | ExprKind::Paren(x)
        | ExprKind::Await(x)
        | ExprKind::Member { obj: x, .. } => visit_expr(x, f),
        ExprKind::Yield { arg, .. } => {
            if let Some(x) = arg {
                visit_expr(x, f);
            }
        }
        ExprKind::Binary(_, a, b) | ExprKind::Logical(_, a, b) | ExprKind::Assign(_, a, b) | ExprKind::Index { obj: a, index: b, .. } => {
            visit_expr(a, f);
            visit_expr(b, f);
        }
        ExprKind::Cond(a, b, c) => {
            visit_expr(a, f);
            visit_expr(b, f);
            visit_expr(c, f);
        }
        ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args, .. } => {
            visit_expr(callee, f);
            visit_elems(args, f);
        }
        ExprKind::Seq(xs) => {
            for x in xs {
                visit_expr(x, f);
            }
        }
        ExprKind::Num(_)
        | ExprKind::BigInt(_)
        | ExprKind::Str(_)
        | ExprKind::Regex(..)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::Ident(_) => {}
    }
}
