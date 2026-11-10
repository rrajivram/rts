//! Register bytecode. Registers are frame-relative indices into one of two
//! files: `S` (u64 scalars: f64 bits or 0/1 booleans) and `V` (`Value`s).
//! Every instruction's operands have a fixed file, so the interpreter never
//! checks a tag for typed code.

use std::rc::Rc;

use crate::hir::{CmpOp, NumOp};
use crate::runtime::{ClassRt, Native, PropCache, Shape, Value};
use crate::types::Kind;

pub type R = u16;

#[derive(Debug, Clone, Copy)]
pub enum I {
    // ── scalars ──
    LoadN { d: R, v: f64 },
    LoadB { d: R, v: bool },
    MovS { d: R, s: R },
    AddN { d: R, a: R, b: R },
    SubN { d: R, a: R, b: R },
    MulN { d: R, a: R, b: R },
    DivN { d: R, a: R, b: R },
    ModN { d: R, a: R, b: R },
    /// `d = a + k`
    AddNK { d: R, a: R, k: f64 },
    NumOp { op: NumOp, d: R, a: R, b: R },
    LtN { d: R, a: R, b: R },
    LeN { d: R, a: R, b: R },
    GtN { d: R, a: R, b: R },
    GeN { d: R, a: R, b: R },
    EqN { d: R, a: R, b: R },
    NeN { d: R, a: R, b: R },
    EqB { d: R, a: R, b: R },
    NeB { d: R, a: R, b: R },
    NegN { d: R, s: R },
    NotB { d: R, s: R },
    BitNotN { d: R, s: R },
    TruthyN { d: R, s: R },
    Math1 { op: Native, d: R, s: R },
    Math2 { op: Native, d: R, a: R, b: R },
    // ── control ──
    Jmp { t: u32 },
    JmpT { c: R, t: u32 },
    JmpF { c: R, t: u32 },
    /// Fused numeric compare-and-branch: jump if `(a op b) == sense`.
    JCmpN { op: CmpOp, sense: bool, a: R, b: R, t: u32 },
    JmpTV { c: R, t: u32 },
    JmpFV { c: R, t: u32 },
    JmpNullish { s: R, t: u32 },
    JmpNotNullish { s: R, t: u32 },
    // ── values ──
    LoadK { d: R, k: u32 },
    LoadUndef { d: R },
    LoadNull { d: R },
    MovV { d: R, s: R },
    ClearV { d: R, n: u16 },
    BoxN { d: R, s: R },
    BoxB { d: R, s: R },
    UnboxN { d: R, s: R },
    UnboxB { d: R, s: R },
    TruthyV { d: R, s: R },
    NumToStr { d: R, s: R },
    ToStr { d: R, s: R },
    ToNum { d: R, s: R },
    Concat { d: R, a: R, b: R },
    StrCmp { op: CmpOp, d: R, a: R, b: R },
    StrictEq { d: R, a: R, b: R, eq: bool },
    LooseEq { d: R, a: R, b: R, eq: bool },
    DynAdd { d: R, a: R, b: R },
    DynNum { op: NumOp, d: R, a: R, b: R },
    DynCmp { op: CmpOp, d: R, a: R, b: R },
    TypeOf { d: R, s: R },
    In { d: R, k: R, o: R },
    InstanceOf { d: R, o: R, class: u32 },
    // ── cells, upvalues, globals ──
    NewCell { d: R },
    CellGet { d: R, c: R },
    CellSet { c: R, s: R },
    UpGet { d: R, i: u16 },
    UpSet { i: u16, s: R },
    GetGS { d: R, g: u32 },
    SetGS { g: u32, s: R },
    GetGV { d: R, g: u32 },
    SetGV { g: u32, s: R },
    // ── functions ──
    MakeClosure { d: R, f: u32, caps: u32 },
    Call { ci: u32 },
    RetN { s: R },
    RetB { s: R },
    RetV { s: R },
    RetUndef,
    // ── arrays & strings ──
    NewArr { d: R, num: bool },
    PushN { a: R, s: R },
    PushV { a: R, s: R },
    PushSpread { a: R, s: R },
    GetIdxN { d: R, a: R, i: R },
    SetIdxN { a: R, i: R, s: R },
    GetIdxV { d: R, a: R, i: R },
    SetIdxV { a: R, i: R, s: R },
    GetDyn { d: R, o: R, k: R },
    SetDyn { o: R, k: R, s: R },
    DelDyn { o: R, k: R },
    ArrLen { d: R, a: R },
    SetArrLen { a: R, s: R },
    StrLen { d: R, s: R },
    CharCodeAt { d: R, s: R, i: R },
    /// Elements a for-of loop visits, as an array (arrays pass through).
    IterArr { d: R, s: R },
    KeysArr { d: R, s: R },
    // ── objects ──
    NewObj { d: R, shape: u32 },
    ObjSpread { o: R, s: R },
    GetField { d: R, o: R, i: u16 },
    SetField { o: R, i: u16, s: R },
    GetFieldN { d: R, o: R, i: u16 },
    SetFieldN { o: R, i: u16, s: R },
    GetProp { d: R, o: R, p: u32 },
    SetProp { o: R, p: u32, s: R },
    NewInst { d: R, class: u32 },
    LoadClass { d: R, class: u32 },
    // ── exceptions ──
    Throw { s: R },
    TryBegin { catch: u32, exc: R },
    TryEnd,
}

#[derive(Debug, Clone, Copy)]
pub enum CapSrc {
    /// A V register of the enclosing frame holding a cell.
    Local(R),
    /// The enclosing closure's own capture.
    Up(u16),
}

#[derive(Debug)]
pub enum CallTarget {
    Direct(u32),
    Value(R),
    /// Virtual call through the receiver's class vtable.
    Method(u32),
    /// Method/function property looked up by name on the receiver.
    Prop(u32),
    Native(Native),
}

#[derive(Debug)]
pub struct CallInfo {
    pub target: CallTarget,
    /// Receiver (V register).
    pub this: Option<R>,
    pub args: Vec<(Kind, R)>,
    /// Arguments contain spread markers: (Val, reg) entries flagged here are spread.
    pub spread: Vec<bool>,
    pub has_spread: bool,
    pub dst: Option<(Kind, R)>,
    /// The caller already packed rest arguments into an array.
    pub packed_rest: bool,
}

#[derive(Debug)]
pub struct PropSite {
    pub name: Rc<str>,
    pub key: Value,
    pub cache: PropCache,
}

#[derive(Debug, Default)]
pub struct FuncCode {
    pub name: Rc<str>,
    pub code: Vec<I>,
    pub ns: usize,
    pub nv: usize,
    /// Arrival kinds of parameters (after `this`, which is V0).
    pub params: Vec<Kind>,
    pub rest: bool,
    pub consts: Vec<Value>,
    pub calls: Vec<CallInfo>,
    pub props: Vec<PropSite>,
    pub caps: Vec<Vec<CapSrc>>,
    /// Source line per instruction (for error messages) — reserved.
    pub lines: Vec<u32>,
}

#[derive(Debug)]
pub struct Program {
    pub funcs: Vec<Rc<FuncCode>>,
    pub classes: Vec<Rc<ClassRt>>,
    pub shapes: Vec<Rc<Shape>>,
    pub n_gs: usize,
    pub n_gv: usize,
    pub main: u32,
    pub error_class: Option<u32>,
    pub type_error_class: Option<u32>,
    pub range_error_class: Option<u32>,
    pub syntax_error_class: Option<u32>,
}
