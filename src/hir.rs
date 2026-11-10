//! Typed intermediate representation: the checker's output, codegen's input.
//!
//! Every expression carries its (possibly narrowed) static type; every name is
//! resolved. Codegen inserts kind conversions (box/unbox) wherever the kind of
//! an expression differs from the kind its consumer needs.

use std::rc::Rc;

use crate::ast::{LogicalOp, Name};
use crate::runtime::natives::Native;
use crate::types::{ClassId, Kind, Type};

pub type VarId = u32;
pub type FuncId = u32;

#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: Name,
    /// Declared (storage) type; determines the register kind.
    pub ty: Type,
    /// Function that owns the variable; None = global.
    pub owner: Option<FuncId>,
    /// Referenced from a nested function → lives in a cell.
    pub captured: bool,
}

impl VarInfo {
    pub fn kind(&self) -> Kind {
        self.ty.kind()
    }
}

#[derive(Debug, Clone)]
pub struct HParam {
    pub var: VarId,
    /// Kind the argument arrives in (Val for optional/defaulted/rest params).
    pub arrival: Kind,
    pub default: Option<HExpr>,
    pub rest: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HFunc {
    pub name: Name,
    pub this_var: Option<VarId>,
    pub params: Vec<HParam>,
    pub ret_kind: Option<Kind>,
    pub body: Vec<HStmt>,
    /// Variables owned by enclosing functions that this function (or a nested one) uses.
    pub free: Vec<VarId>,
    pub parent: Option<FuncId>,
    /// Destructuring of parameters etc. happens in the body; nothing else here.
    pub is_ctor: bool,
    pub class: Option<ClassId>,
}

#[derive(Debug, Clone)]
pub struct HClass {
    pub name: Name,
    pub parent: Option<ClassId>,
    pub fields: Vec<Name>,
    /// Method name → function, including inherited (vtable order = index).
    pub vtable: Vec<(Name, FuncId)>,
    pub ctor: FuncId,
}

#[derive(Debug, Clone)]
pub struct HProgram {
    pub funcs: Vec<HFunc>,
    pub vars: Vec<VarInfo>,
    pub classes: Vec<HClass>,
    pub main: FuncId,
    /// Class ids of the prelude Error classes, for runtime-raised errors.
    pub error_class: Option<ClassId>,
    pub type_error_class: Option<ClassId>,
    pub range_error_class: Option<ClassId>,
}

// ───────────────────────────── statements ─────────────────────────────

#[derive(Debug, Clone)]
pub enum HStmt {
    Expr(HExpr),
    /// Initialize a variable (declaration). `None` = undefined / zero.
    Init(VarId, Option<HExpr>),
    /// Create closures for hoisted function declarations of a block.
    MakeClosure(VarId, FuncId),
    Block(Vec<HStmt>, Vec<VarId>),
    If(HExpr, Vec<HStmt>, Vec<HStmt>),
    Loop {
        label: Option<Name>,
        /// Evaluated before each iteration; loop exits when false.
        test: Option<HExpr>,
        body: Vec<HStmt>,
        update: Option<HExpr>,
        /// do-while: run body once before the first test.
        test_after: bool,
        /// Captured per-iteration loop variables to re-cell each iteration.
        fresh: Vec<VarId>,
    },
    /// `for (const x of iterable)`; `elem` receives each element.
    ForOf {
        label: Option<Name>,
        elem: VarId,
        iter: HExpr,
        body: Vec<HStmt>,
    },
    ForIn {
        label: Option<Name>,
        key: VarId,
        obj: HExpr,
        body: Vec<HStmt>,
    },
    Break(Option<Name>),
    Continue(Option<Name>),
    Return(Option<HExpr>),
    Throw(HExpr),
    Try {
        block: Vec<HStmt>,
        catch_var: Option<VarId>,
        handler: Vec<HStmt>,
        finalizer: Option<Vec<HStmt>>,
    },
    Switch {
        label: Option<Name>,
        disc: HExpr,
        cases: Vec<(Option<HExpr>, Vec<HStmt>)>,
    },
    Labeled(Name, Box<HStmt>),
}

// ───────────────────────────── expressions ─────────────────────────────

#[derive(Debug, Clone)]
pub struct HExpr {
    pub kind: HK,
    pub ty: Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl CmpOp {
    pub fn negate(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HBin {
    /// Both operands numbers (scalar).
    Num(NumOp),
    /// Numeric comparison, both scalar numbers. (Negation of NaN compares isn't
    /// valid, so fused jumps only negate via jump sense, never the op.)
    NumCmp(CmpOp),
    /// Booleans `===` / `!==` (scalar).
    BoolEq(bool),
    /// String concatenation (both strings).
    Concat,
    /// String comparison `<` etc. (both strings).
    StrCmp(CmpOp),
    /// `===`/`!==` on values (true = equal).
    StrictEq(bool),
    /// `==`/`!=` on values.
    LooseEq(bool),
    /// Fully dynamic `+` (number or string at run time).
    DynAdd,
    /// Dynamic arithmetic: operands converted with ToNumber.
    DynNum(NumOp),
    /// Dynamic relational comparison.
    DynCmp(CmpOp),
    /// `key in obj`
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HUnary {
    NegNum,
    NotBool,
    BitNotNum,
    /// `typeof x` → string
    TypeOf,
    /// `void x` → undefined
    Void,
    /// Truthiness of any value → boolean
    Truthy,
    /// ToNumber of any value → number
    ToNum,
    /// ToString of any value (template literals, String(x)) → string
    ToStr,
}

#[derive(Debug, Clone)]
pub enum Place {
    Var(VarId),
    /// Class instance field at a fixed slot.
    Field { obj: HExpr, slot: u32, opt: bool },
    /// Property lookup by name (structural types, `any`), with inline cache.
    Prop { obj: HExpr, name: Name, opt: bool },
    /// `arr[i]` with a numeric index on an array.
    Index { obj: HExpr, index: HExpr, opt: bool },
    /// `obj[key]` dynamic (records, any, strings).
    DynIndex { obj: HExpr, key: HExpr, opt: bool },
    /// `arr.length`
    ArrLen { obj: HExpr, opt: bool },
    /// `str.length` (read only)
    StrLen { obj: HExpr, opt: bool },
}

#[derive(Debug, Clone)]
pub enum HElem {
    Expr(HExpr),
    Spread(HExpr),
}

#[derive(Debug, Clone)]
pub enum HProp {
    Init(Name, HExpr),
    Spread(HExpr),
}

#[derive(Debug, Clone)]
pub enum Callee {
    /// Known function, no captures needed (or captures via `closure` var).
    Direct { func: FuncId, closure: Option<VarId> },
    /// Any function value (closure) of known or unknown signature.
    Value(HExpr),
    /// Virtual method call on a class instance.
    Method { class: ClassId, vslot: u32 },
    /// Method looked up by name on the receiver at run time.
    Prop(Name),
    /// Built-in.
    Native(Native),
}

#[derive(Debug, Clone)]
pub struct HCall {
    pub callee: Callee,
    /// Receiver (`this`) for methods / natives.
    pub this: Option<HExpr>,
    pub args: Vec<HElem>,
    /// Argument kinds the callee expects (for Direct/Method calls exactly; for
    /// Value calls the static signature's kinds; empty → natural kinds).
    pub param_kinds: Option<Vec<Kind>>,
    /// Callee has a rest parameter at this position (args from here are packed).
    pub rest_at: Option<usize>,
    /// The call result's kind as returned by the callee (before coercion).
    pub ret_kind: Kind,
    pub opt: bool,
}

#[derive(Debug, Clone)]
pub enum HK {
    Num(f64),
    Bool(bool),
    Str(Rc<str>),
    Null,
    Undefined,
    Read(Box<Place>),
    /// Closure creation (function expressions, arrows).
    Closure(FuncId),
    Array(Vec<HElem>),
    Object(Vec<HProp>),
    Unary(HUnary, Box<HExpr>),
    Binary(HBin, Box<HExpr>, Box<HExpr>),
    Logical(LogicalOp, Box<HExpr>, Box<HExpr>),
    Cond(Box<HExpr>, Box<HExpr>, Box<HExpr>),
    Assign(Box<Place>, Box<HExpr>),
    /// `place op= value`; `op` operates on the place's value and `value`.
    Compound(Box<Place>, HBin, Box<HExpr>),
    LogicalAssign(Box<Place>, LogicalOp, Box<HExpr>),
    Update {
        place: Box<Place>,
        inc: bool,
        prefix: bool,
    },
    Call(Box<HCall>),
    New {
        class: ClassId,
        args: Vec<HElem>,
    },
    /// `new Map()`, `new Set(xs)`, `new Array(n)`.
    NewBuiltin(Native, Vec<HElem>),
    InstanceOf(Box<HExpr>, ClassId),
    /// Static re-typing (as, !, narrowing). Codegen coerces kinds.
    Cast(Box<HExpr>),
    /// Short-circuit boundary for `?.`
    OptChain(Box<HExpr>),
    Seq(Vec<HExpr>),
    /// Class constructor as a first-class value.
    ClassRef(ClassId),
    /// A built-in function as a value (`const log = console.log`).
    NativeFn(Native),
    /// `delete obj[key]`
    Delete(Box<HExpr>, Box<HExpr>),
}

impl HExpr {
    pub fn new(kind: HK, ty: Type) -> Self {
        HExpr { kind, ty }
    }
    pub fn undefined() -> Self {
        HExpr::new(HK::Undefined, Type::Undefined)
    }
    pub fn num(v: f64) -> Self {
        HExpr::new(HK::Num(v), Type::Number)
    }
    pub fn str(s: &str) -> Self {
        HExpr::new(HK::Str(s.into()), Type::String)
    }
    pub fn read(place: Place, ty: Type) -> Self {
        HExpr::new(HK::Read(Box::new(place)), ty)
    }
    pub fn cast(self, ty: Type) -> Self {
        if self.ty == ty {
            return self;
        }
        HExpr::new(HK::Cast(Box::new(self)), ty)
    }
    pub fn kind(&self) -> Kind {
        self.ty.kind()
    }
}
