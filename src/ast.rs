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
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    /// `123n`
    BigInt(i128),
    Str(Name),
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
}
