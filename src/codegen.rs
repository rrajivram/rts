//! HIR → bytecode. Allocates typed registers (S for numbers/booleans, V for
//! values) and inserts box/unbox at every point where kinds differ.

use std::collections::HashMap;
use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::{LogicalOp, Name};
use crate::bytecode::*;
use crate::hir::*;
use crate::runtime::{ClassRt, Native, PropCache, Shape, Value};
use crate::types::{Kind, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Op {
    k: Kind,
    r: R,
}

#[derive(Debug, Clone, Copy)]
enum Home {
    S(R),
    V(R),
    /// V register holding the variable's cell.
    Cell(R),
    Up(u16),
    GS(u32),
    GV(u32),
}

/// An evaluated assignment target.
enum PlaceRef {
    Var(VarId),
    Field { o: R, slot: u16, k: Kind },
    Prop { o: R, p: u32 },
    Index { a: R, i: R, k: Kind },
    Dyn { o: R, k: R },
    ArrLen { a: R },
    StrLen { s: R },
}

struct LoopCtx {
    label: Option<Name>,
    is_switch: bool,
    breaks: Vec<usize>,
    continues: Vec<usize>,
    try_depth: usize,
}

struct TryCtx {
    finalizer: Option<Vec<HStmt>>,
    /// Has an active TryBegin handler that must be popped when jumping out.
    has_handler: bool,
}

pub fn compile(hir: &HProgram) -> Program {
    let mut cg = Codegen {
        hir,
        shapes: Vec::new(),
        shape_ids: HashMap::new(),
        globals: HashMap::new(),
        n_gs: 0,
        n_gv: 0,
    };
    for (i, v) in hir.vars.iter().enumerate() {
        if v.owner.is_none() {
            let h = if v.kind().is_scalar() {
                cg.n_gs += 1;
                Home::GS(cg.n_gs as u32 - 1)
            } else {
                cg.n_gv += 1;
                Home::GV(cg.n_gv as u32 - 1)
            };
            cg.globals.insert(i as VarId, h);
        }
    }
    let funcs: Vec<Rc<FuncCode>> = (0..hir.funcs.len()).map(|f| Rc::new(cg.func(f as FuncId))).collect();
    let mut classes: Vec<Option<Rc<ClassRt>>> = vec![None; hir.classes.len()];
    fn build(i: usize, hir: &HProgram, out: &mut Vec<Option<Rc<ClassRt>>>) -> Rc<ClassRt> {
        if let Some(c) = &out[i] {
            return c.clone();
        }
        let hc = &hir.classes[i];
        let parent = hc.parent.map(|p| build(p as usize, hir, out));
        let mut methods = FxHashMap::default();
        for (n, f) in &hc.vtable {
            methods.insert(n.clone(), *f);
        }
        let c = Rc::new(ClassRt {
            id: i as u32,
            name: hc.name.clone(),
            parent,
            shape: Shape::new(hc.fields.clone()),
            vtable: hc.vtable.iter().map(|(_, f)| *f).collect(),
            methods,
            ctor: hc.ctor,
        });
        out[i] = Some(c.clone());
        c
    }
    for i in 0..hir.classes.len() {
        build(i, hir, &mut classes);
    }
    Program {
        funcs,
        classes: classes.into_iter().map(|c| c.unwrap()).collect(),
        shapes: cg.shapes,
        n_gs: cg.n_gs,
        n_gv: cg.n_gv,
        main: hir.main,
        error_class: hir.error_class,
        type_error_class: hir.type_error_class,
        range_error_class: hir.range_error_class,
        syntax_error_class: None,
    }
}

struct Codegen<'a> {
    hir: &'a HProgram,
    shapes: Vec<Rc<Shape>>,
    shape_ids: HashMap<Vec<Rc<str>>, u32>,
    globals: HashMap<VarId, Home>,
    n_gs: usize,
    n_gv: usize,
}

impl<'a> Codegen<'a> {
    fn shape(&mut self, keys: Vec<Rc<str>>) -> u32 {
        if let Some(i) = self.shape_ids.get(&keys) {
            return *i;
        }
        let i = self.shapes.len() as u32;
        self.shapes.push(Shape::new(keys.clone()));
        self.shape_ids.insert(keys, i);
        i
    }

    fn func(&mut self, fid: FuncId) -> FuncCode {
        let hf = &self.hir.funcs[fid as usize];
        let mut g = FnGen {
            cg: self,
            fid,
            code: Vec::new(),
            s_top: 0,
            v_top: 1,
            max_s: 0,
            max_v: 1,
            consts: Vec::new(),
            str_consts: HashMap::new(),
            calls: Vec::new(),
            props: Vec::new(),
            caps: Vec::new(),
            homes: HashMap::new(),
            loops: Vec::new(),
            tries: Vec::new(),
            opt_exits: Vec::new(),
        };
        // Arrival registers: V0 = this, then params in order per file.
        let mut arrivals = Vec::new();
        for p in &hf.params {
            let r = match p.arrival {
                Kind::Val => g.alloc_v(),
                _ => g.alloc_s(),
            };
            arrivals.push(r);
        }
        for (i, v) in hf.free.iter().enumerate() {
            g.homes.insert(*v, Home::Up(i as u16));
        }
        if let Some(tv) = hf.this_var {
            g.bind_arrival(tv, Kind::Val, 0);
        }
        for (p, r) in hf.params.iter().zip(arrivals) {
            if let Some(d) = &p.default {
                let skip = g.jump_placeholder();
                g.code[skip] = I::JmpNotNullish { s: r, t: 0 };
                let m = g.mark();
                let dv = g.expr(d);
                let dv = g.coerce(dv, Kind::Val);
                g.emit(I::MovV { d: r, s: dv.r });
                g.reset(m);
                g.patch_here(skip);
            }
            g.bind_arrival(p.var, p.arrival, r);
        }
        for st in &hf.body {
            g.stmt(st);
        }
        g.emit(I::RetUndef);
        let name = hf.name.clone();
        FuncCode {
            name,
            code: g.code,
            ns: g.max_s as usize,
            nv: g.max_v as usize,
            params: hf.params.iter().map(|p| p.arrival).collect(),
            rest: hf.params.last().is_some_and(|p| p.rest),
            consts: g.consts,
            calls: g.calls,
            props: g.props,
            caps: g.caps,
            lines: Vec::new(),
        }
    }
}

struct FnGen<'c, 'a> {
    cg: &'c mut Codegen<'a>,
    fid: FuncId,
    code: Vec<I>,
    s_top: u16,
    v_top: u16,
    max_s: u16,
    max_v: u16,
    consts: Vec<Value>,
    str_consts: HashMap<Rc<str>, u32>,
    calls: Vec<CallInfo>,
    props: Vec<PropSite>,
    caps: Vec<Vec<CapSrc>>,
    homes: HashMap<VarId, Home>,
    loops: Vec<LoopCtx>,
    tries: Vec<TryCtx>,
    opt_exits: Vec<Vec<usize>>,
}

impl FnGen<'_, '_> {
    // ───────────── registers & emission ─────────────

    fn alloc_s(&mut self) -> R {
        let r = self.s_top;
        self.s_top += 1;
        self.max_s = self.max_s.max(self.s_top);
        r
    }
    fn alloc_v(&mut self) -> R {
        let r = self.v_top;
        self.v_top += 1;
        self.max_v = self.max_v.max(self.v_top);
        r
    }
    fn alloc(&mut self, k: Kind) -> Op {
        Op {
            k,
            r: if k == Kind::Val { self.alloc_v() } else { self.alloc_s() },
        }
    }
    fn mark(&self) -> (u16, u16) {
        (self.s_top, self.v_top)
    }
    fn reset(&mut self, m: (u16, u16)) {
        self.s_top = m.0;
        self.v_top = m.1;
    }
    fn emit(&mut self, i: I) -> usize {
        self.code.push(i);
        self.code.len() - 1
    }
    fn here(&self) -> u32 {
        self.code.len() as u32
    }
    fn jump_placeholder(&mut self) -> usize {
        self.emit(I::Jmp { t: u32::MAX })
    }
    fn patch(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            I::Jmp { t }
            | I::JmpT { t, .. }
            | I::JmpF { t, .. }
            | I::JCmpN { t, .. }
            | I::JmpTV { t, .. }
            | I::JmpFV { t, .. }
            | I::JmpNullish { t, .. }
            | I::JmpNotNullish { t, .. } => *t = target,
            I::TryBegin { catch, .. } => *catch = target,
            other => panic!("patching non-jump {other:?}"),
        }
    }
    fn patch_here(&mut self, at: usize) {
        let h = self.here();
        self.patch(at, h);
    }
    fn konst(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        self.consts.len() as u32 - 1
    }
    fn str_const(&mut self, s: &Rc<str>) -> u32 {
        if let Some(k) = self.str_consts.get(s) {
            return *k;
        }
        let k = self.konst(Value::str(&**s));
        self.str_consts.insert(s.clone(), k);
        k
    }
    fn prop_site(&mut self, name: &Rc<str>) -> u32 {
        self.props.push(PropSite {
            name: name.clone(),
            key: Value::str(&**name),
            cache: PropCache::default(),
        });
        self.props.len() as u32 - 1
    }

    fn var_info(&self, v: VarId) -> &VarInfo {
        &self.cg.hir.vars[v as usize]
    }

    fn home(&self, v: VarId) -> Home {
        if let Some(h) = self.homes.get(&v) {
            return *h;
        }
        if let Some(h) = self.cg.globals.get(&v) {
            return *h;
        }
        panic!(
            "codegen: variable {} ({}) has no home in function {}",
            v,
            self.var_info(v).name,
            self.cg.hir.funcs[self.fid as usize].name
        )
    }

    /// Give a parameter/this variable its home given its arrival register.
    fn bind_arrival(&mut self, var: VarId, arrival: Kind, r: R) {
        let info = self.var_info(var).clone();
        if info.captured {
            let c = self.alloc_v();
            self.emit(I::NewCell { d: c });
            let val = self.coerce(Op { k: arrival, r }, Kind::Val);
            self.emit(I::CellSet { c, s: val.r });
            self.homes.insert(var, Home::Cell(c));
            return;
        }
        let k = info.kind();
        if k == arrival {
            self.homes.insert(var, if k == Kind::Val { Home::V(r) } else { Home::S(r) });
        } else {
            let h = self.alloc(k);
            let src = self.coerce(Op { k: arrival, r }, k);
            self.mov(h, src);
            self.homes.insert(var, if k == Kind::Val { Home::V(h.r) } else { Home::S(h.r) });
        }
    }

    /// Allocate a home for a newly declared local.
    fn declare(&mut self, var: VarId) -> Home {
        if let Some(h) = self.cg.globals.get(&var) {
            return *h;
        }
        let info = self.var_info(var);
        let h = if info.captured {
            Home::Cell(self.alloc_v())
        } else if info.kind() == Kind::Val {
            Home::V(self.alloc_v())
        } else {
            Home::S(self.alloc_s())
        };
        self.homes.insert(var, h);
        h
    }

    fn mov(&mut self, d: Op, s: Op) {
        debug_assert_eq!(d.k == Kind::Val, s.k == Kind::Val);
        if d.r == s.r && d.k == s.k {
            return;
        }
        if d.k == Kind::Val {
            self.emit(I::MovV { d: d.r, s: s.r });
        } else {
            self.emit(I::MovS { d: d.r, s: s.r });
        }
    }

    /// Convert an operand to another kind (boxing / checked unboxing).
    fn coerce(&mut self, o: Op, k: Kind) -> Op {
        if o.k == k {
            return o;
        }
        match (o.k, k) {
            (Kind::Num, Kind::Val) => {
                let d = self.alloc_v();
                self.emit(I::BoxN { d, s: o.r });
                Op { k, r: d }
            }
            (Kind::Bool, Kind::Val) => {
                let d = self.alloc_v();
                self.emit(I::BoxB { d, s: o.r });
                Op { k, r: d }
            }
            (Kind::Val, Kind::Num) => {
                let d = self.alloc_s();
                self.emit(I::UnboxN { d, s: o.r });
                Op { k, r: d }
            }
            (Kind::Val, Kind::Bool) => {
                let d = self.alloc_s();
                self.emit(I::UnboxB { d, s: o.r });
                Op { k, r: d }
            }
            // number <-> boolean only through casts; go via a value (checked).
            (a, b) => {
                let v = self.coerce(Op { k: a, r: o.r }, Kind::Val);
                self.coerce(v, b)
            }
        }
    }

    // ───────────── statements ─────────────

    fn stmts(&mut self, ss: &[HStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn block(&mut self, ss: &[HStmt]) {
        self.block_vars(ss, &[]);
    }

    /// A block: allocate homes for its declarations (cells for captured ones)
    /// up front, so closures created anywhere in the block see the same cells.
    fn block_vars(&mut self, ss: &[HStmt], vars: &[VarId]) {
        let m = self.mark();
        for v in vars {
            if self.cg.globals.contains_key(v) {
                continue;
            }
            self.homes.remove(v);
            if let Home::Cell(c) = self.declare(*v) {
                self.emit(I::NewCell { d: c });
            }
        }
        self.stmts(ss);
        if self.v_top > m.1 {
            self.emit(I::ClearV { d: m.1, n: self.v_top - m.1 });
        }
        self.reset(m);
    }

    /// Default value for an uninitialized declaration (`let x: number;`).
    fn init_default(&mut self, var: VarId) {
        let k = self.var_info(var).kind();
        let t = self.alloc(k);
        match k {
            Kind::Num => self.emit(I::LoadN { d: t.r, v: f64::NAN }),
            Kind::Bool => self.emit(I::LoadB { d: t.r, v: false }),
            Kind::Val => self.emit(I::LoadUndef { d: t.r }),
        };
        self.store_var(var, t);
    }

    /// Bind a fresh variable (for-of element, catch parameter): new cell if captured.
    fn init_var(&mut self, var: VarId, val: Option<Op>) {
        let h = match self.homes.get(&var) {
            Some(Home::Cell(c)) => Home::Cell(*c),
            Some(h) => *h,
            None => self.declare(var),
        };
        let k = self.var_info(var).kind();
        match h {
            Home::Cell(c) => {
                self.emit(I::NewCell { d: c });
                if let Some(v) = val {
                    let v = self.coerce(v, Kind::Val);
                    self.emit(I::CellSet { c, s: v.r });
                } else if k == Kind::Num {
                    let t = self.alloc_v();
                    let kk = self.konst(Value::Num(f64::NAN));
                    self.emit(I::LoadK { d: t, k: kk });
                    self.emit(I::CellSet { c, s: t });
                } else if k == Kind::Bool {
                    let t = self.alloc_v();
                    let kk = self.konst(Value::Bool(false));
                    self.emit(I::LoadK { d: t, k: kk });
                    self.emit(I::CellSet { c, s: t });
                }
            }
            _ => match val {
                Some(v) => self.store_var(var, v),
                None => match (h, k) {
                    (Home::S(r), Kind::Num) => {
                        self.emit(I::LoadN { d: r, v: f64::NAN });
                    }
                    (Home::S(r), _) => {
                        self.emit(I::LoadB { d: r, v: false });
                    }
                    (Home::V(r), _) => {
                        self.emit(I::LoadUndef { d: r });
                    }
                    (Home::GS(_), _) | (Home::GV(_), _) => {
                        let t = self.alloc(k);
                        match k {
                            Kind::Num => self.emit(I::LoadN { d: t.r, v: f64::NAN }),
                            Kind::Bool => self.emit(I::LoadB { d: t.r, v: false }),
                            Kind::Val => self.emit(I::LoadUndef { d: t.r }),
                        };
                        self.store_var(var, t);
                    }
                    _ => {}
                },
            },
        }
    }

    fn stmt(&mut self, st: &HStmt) {
        let m = self.mark();
        match st {
            HStmt::Expr(e) => {
                self.expr_discard(e);
                self.reset(m);
            }
            HStmt::Init(var, init) => {
                // Homes are normally created by the enclosing block's prologue.
                if !self.homes.contains_key(var) {
                    if let Home::Cell(c) = self.declare(*var) {
                        self.emit(I::NewCell { d: c });
                    }
                }
                let m2 = self.mark();
                let k = self.var_info(*var).kind();
                match (init, self.home(*var)) {
                    (Some(e), Home::S(r) | Home::V(r)) => self.expr_into(e, Op { k, r }),
                    (Some(e), _) => {
                        let v = self.expr(e);
                        self.store_var(*var, v);
                    }
                    (None, _) => self.init_default(*var),
                }
                self.reset(m2);
            }
            HStmt::MakeClosure(var, f) => {
                if !self.homes.contains_key(var) {
                    if let Home::Cell(c) = self.declare(*var) {
                        self.emit(I::NewCell { d: c });
                    }
                }
                let m2 = self.mark();
                let c = self.closure(*f);
                self.store_var(*var, c);
                self.reset(m2);
            }
            HStmt::Block(ss, vars) => self.block_vars(ss, vars),
            HStmt::If(c, t, e) => {
                let else_l = self.cond_jump(c, false);
                self.reset(m);
                self.block(t);
                if e.is_empty() {
                    self.patch_all_here(else_l);
                } else {
                    let end = self.jump_placeholder();
                    self.patch_all_here(else_l);
                    self.block(e);
                    self.patch_here(end);
                }
            }
            HStmt::Loop {
                label,
                test,
                body,
                update,
                test_after,
                fresh,
            } => {
                self.loops.push(LoopCtx {
                    label: label.clone(),
                    is_switch: false,
                    breaks: vec![],
                    continues: vec![],
                    try_depth: self.tries.len(),
                });
                let first = if *test_after { Some(self.jump_placeholder()) } else { None };
                let top = self.here();
                let mut exits = Vec::new();
                if let Some(t) = test {
                    let mm = self.mark();
                    exits = self.cond_jump(t, false);
                    self.reset(mm);
                }
                if let Some(f) = first {
                    self.patch_here(f);
                }
                self.block(body);
                let cont = self.here();
                // Fresh per-iteration bindings for captured loop variables.
                for v in fresh {
                    if let Some(Home::Cell(c)) = self.homes.get(v).copied() {
                        let t = self.alloc_v();
                        self.emit(I::CellGet { d: t, c });
                        self.emit(I::NewCell { d: c });
                        self.emit(I::CellSet { c, s: t });
                        self.v_top -= 1;
                    }
                }
                if let Some(u) = update {
                    let mm = self.mark();
                    self.expr_discard(u);
                    self.reset(mm);
                }
                self.emit(I::Jmp { t: top });
                let ctx = self.loops.pop().unwrap();
                for c in ctx.continues {
                    self.patch(c, cont);
                }
                for e in exits {
                    self.patch_here(e);
                }
                for b in ctx.breaks {
                    self.patch_here(b);
                }
            }
            HStmt::ForOf { label, elem, iter, body } | HStmt::ForIn {
                label,
                key: elem,
                obj: iter,
                body,
            } => {
                let is_in = matches!(st, HStmt::ForIn { .. });
                let src = self.expr(iter);
                let src = self.coerce(src, Kind::Val);
                let arr = self.alloc_v();
                if is_in {
                    self.emit(I::KeysArr { d: arr, s: src.r });
                } else {
                    self.emit(I::IterArr { d: arr, s: src.r });
                }
                let idx = self.alloc_s();
                self.emit(I::LoadN { d: idx, v: 0.0 });
                let len = self.alloc_s();
                self.loops.push(LoopCtx {
                    label: label.clone(),
                    is_switch: false,
                    breaks: vec![],
                    continues: vec![],
                    try_depth: self.tries.len(),
                });
                let top = self.here();
                self.emit(I::ArrLen { d: len, a: arr });
                let exit = self.emit(I::JCmpN {
                    op: CmpOp::Lt,
                    sense: false,
                    a: idx,
                    b: len,
                    t: 0,
                });
                let bm = self.mark();
                let ek = self.var_info(*elem).kind();
                let el = self.alloc(if ek == Kind::Num { Kind::Num } else { Kind::Val });
                if el.k == Kind::Num {
                    self.emit(I::GetIdxN { d: el.r, a: arr, i: idx });
                } else {
                    self.emit(I::GetIdxV { d: el.r, a: arr, i: idx });
                }
                let el = self.coerce(el, ek);
                self.homes.remove(elem);
                self.init_var(*elem, Some(el));
                self.block(body);
                self.reset(bm);
                let cont = self.here();
                self.emit(I::AddNK { d: idx, a: idx, k: 1.0 });
                self.emit(I::Jmp { t: top });
                let ctx = self.loops.pop().unwrap();
                for c in ctx.continues {
                    self.patch(c, cont);
                }
                self.patch_here(exit);
                for b in ctx.breaks {
                    self.patch_here(b);
                }
                self.emit(I::LoadUndef { d: arr });
                self.reset(m);
            }
            HStmt::Break(label) => {
                let i = self
                    .loops
                    .iter()
                    .rposition(|l| match label {
                        Some(n) => l.label.as_ref() == Some(n),
                        None => true,
                    })
                    .expect("break target");
                let depth = self.loops[i].try_depth;
                self.exit_tries(depth);
                let j = self.jump_placeholder();
                self.loops[i].breaks.push(j);
            }
            HStmt::Continue(label) => {
                let i = self
                    .loops
                    .iter()
                    .rposition(|l| {
                        !l.is_switch
                            && match label {
                                Some(n) => l.label.as_ref() == Some(n),
                                None => true,
                            }
                    })
                    .expect("continue target");
                let depth = self.loops[i].try_depth;
                self.exit_tries(depth);
                let j = self.jump_placeholder();
                self.loops[i].continues.push(j);
            }
            HStmt::Return(v) => {
                let rk = self.cg.hir.funcs[self.fid as usize].ret_kind;
                let val = v.as_ref().map(|e| {
                    let o = self.expr(e);
                    match rk {
                        Some(k) => self.coerce(o, k),
                        None => self.coerce(o, Kind::Val),
                    }
                });
                // Keep the value safe from finalizer temporaries.
                let val = val.map(|o| {
                    let t = self.alloc(o.k);
                    self.mov(t, o);
                    t
                });
                self.exit_tries(0);
                match val {
                    None => self.emit(I::RetUndef),
                    Some(o) => match o.k {
                        Kind::Num => self.emit(I::RetN { s: o.r }),
                        Kind::Bool => self.emit(I::RetB { s: o.r }),
                        Kind::Val => self.emit(I::RetV { s: o.r }),
                    },
                };
                self.reset(m);
            }
            HStmt::Throw(e) => {
                let o = self.expr(e);
                let o = self.coerce(o, Kind::Val);
                self.emit(I::Throw { s: o.r });
                self.reset(m);
            }
            HStmt::Try {
                block,
                catch_var,
                handler,
                finalizer,
            } => self.try_stmt(block, *catch_var, handler, finalizer.as_ref()),
            HStmt::Switch { label, disc, cases } => self.switch(label, disc, cases),
            HStmt::Labeled(name, body) => {
                self.loops.push(LoopCtx {
                    label: Some(name.clone()),
                    is_switch: true,
                    breaks: vec![],
                    continues: vec![],
                    try_depth: self.tries.len(),
                });
                self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                for b in ctx.breaks {
                    self.patch_here(b);
                }
            }
        }
    }

    fn patch_all_here(&mut self, js: Vec<usize>) {
        for j in js {
            self.patch_here(j);
        }
    }

    /// Leave try regions above `depth` (innermost first): pop handlers, run finalizers.
    fn exit_tries(&mut self, depth: usize) {
        let mut i = self.tries.len();
        while i > depth {
            i -= 1;
            if self.tries[i].has_handler {
                self.emit(I::TryEnd);
            }
            if let Some(f) = self.tries[i].finalizer.clone() {
                // Run the finalizer outside its own region.
                let saved: Vec<TryCtx> = self.tries.drain(i..).collect();
                self.block(&f);
                self.tries.extend(saved);
            }
        }
    }

    fn try_stmt(&mut self, block: &[HStmt], catch_var: Option<VarId>, handler: &[HStmt], finalizer: Option<&Vec<HStmt>>) {
        let exc = self.alloc_v();
        let begin = self.emit(I::TryBegin { catch: 0, exc });
        self.tries.push(TryCtx {
            finalizer: finalizer.cloned(),
            has_handler: true,
        });
        self.block(block);
        self.tries.pop();
        self.emit(I::TryEnd);
        if let Some(f) = finalizer {
            self.block(f);
        }
        let end = self.jump_placeholder();
        self.patch_here(begin);
        let has_handler = catch_var.is_some() || !handler.is_empty() || finalizer.is_none();
        let mut end2 = None;
        if has_handler {
            // Protect the handler too when there is a finalizer.
            let (exc2, begin2) = if finalizer.is_some() {
                let exc2 = self.alloc_v();
                let b2 = self.emit(I::TryBegin { catch: 0, exc: exc2 });
                self.tries.push(TryCtx {
                    finalizer: finalizer.cloned(),
                    has_handler: true,
                });
                (Some(exc2), Some(b2))
            } else {
                (None, None)
            };
            let m = self.mark();
            if let Some(cv) = catch_var {
                self.homes.remove(&cv);
                self.init_var(cv, Some(Op { k: Kind::Val, r: exc }));
            }
            self.block(handler);
            self.reset(m);
            if let (Some(exc2), Some(b2)) = (exc2, begin2) {
                self.tries.pop();
                self.emit(I::TryEnd);
                self.block(finalizer.unwrap());
                end2 = Some(self.jump_placeholder());
                self.patch_here(b2);
                self.block(finalizer.unwrap());
                self.emit(I::Throw { s: exc2 });
            }
        } else if let Some(f) = finalizer {
            self.block(f);
            self.emit(I::Throw { s: exc });
        }
        self.patch_here(end);
        if let Some(e2) = end2 {
            self.patch_here(e2);
        }
    }

    fn switch(&mut self, label: &Option<Name>, disc: &HExpr, cases: &[(Option<HExpr>, Vec<HStmt>)]) {
        let m = self.mark();
        let d = self.expr(disc);
        // Keep the discriminant in its own register.
        let dr = self.alloc(d.k);
        self.mov(dr, d);
        let mut case_jumps = Vec::new();
        for (test, _) in cases {
            let Some(t) = test else {
                case_jumps.push(None);
                continue;
            };
            let mm = self.mark();
            let tv = self.expr(t);
            let j = if dr.k == Kind::Num && tv.k == Kind::Num {
                self.emit(I::JCmpN {
                    op: CmpOp::Eq,
                    sense: true,
                    a: dr.r,
                    b: tv.r,
                    t: 0,
                })
            } else {
                let a = self.coerce(dr, Kind::Val);
                let b = self.coerce(tv, Kind::Val);
                let c = self.alloc_s();
                self.emit(I::StrictEq {
                    d: c,
                    a: a.r,
                    b: b.r,
                    eq: true,
                });
                self.emit(I::JmpT { c, t: 0 })
            };
            self.reset(mm);
            case_jumps.push(Some(j));
        }
        let default_jump = self.jump_placeholder();
        self.loops.push(LoopCtx {
            label: label.clone(),
            is_switch: true,
            breaks: vec![],
            continues: vec![],
            try_depth: self.tries.len(),
        });
        let mut has_default = false;
        // All case bodies share one scope (as in JS).
        let bm = self.mark();
        for ((test, body), j) in cases.iter().zip(case_jumps) {
            match j {
                Some(j) => self.patch_here(j),
                None => {
                    has_default = true;
                    self.patch_here(default_jump);
                }
            }
            let _ = test;
            self.stmts(body);
        }
        if self.v_top > bm.1 {
            self.emit(I::ClearV { d: bm.1, n: self.v_top - bm.1 });
        }
        self.reset(bm);
        if !has_default {
            self.patch_here(default_jump);
        }
        let ctx = self.loops.pop().unwrap();
        for b in ctx.breaks {
            self.patch_here(b);
        }
        self.reset(m);
    }

    // ───────────── conditions ─────────────

    /// Emit jumps taken when truthiness(e) == sense; returns the jump sites to patch.
    fn cond_jump(&mut self, e: &HExpr, sense: bool) -> Vec<usize> {
        match &e.kind {
            HK::Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) => {
                let is_and = *op == LogicalOp::And;
                if is_and != sense {
                    // and/false or or/true: either operand decides.
                    let mut j = self.cond_jump(a, sense);
                    j.extend(self.cond_jump(b, sense));
                    j
                } else {
                    // and/true: jump if both truthy.
                    let skip = self.cond_jump(a, !sense);
                    let j = self.cond_jump(b, sense);
                    self.patch_all_here(skip);
                    j
                }
            }
            HK::Unary(HUnary::NotBool, inner) => self.cond_jump(inner, !sense),
            HK::Unary(HUnary::Truthy, inner) => self.cond_jump(inner, sense),
            HK::Bool(b) => {
                if *b == sense {
                    vec![self.jump_placeholder()]
                } else {
                    vec![]
                }
            }
            HK::Binary(HBin::NumCmp(op), a, b) => {
                let m = self.mark();
                let x = self.expr(a);
                let y = self.expr(b);
                let j = self.emit(I::JCmpN {
                    op: *op,
                    sense,
                    a: x.r,
                    b: y.r,
                    t: 0,
                });
                self.reset(m);
                vec![j]
            }
            _ => {
                let m = self.mark();
                let o = self.expr(e);
                let j = match o.k {
                    Kind::Bool => {
                        if sense {
                            self.emit(I::JmpT { c: o.r, t: 0 })
                        } else {
                            self.emit(I::JmpF { c: o.r, t: 0 })
                        }
                    }
                    Kind::Num => {
                        let t = self.alloc_s();
                        self.emit(I::TruthyN { d: t, s: o.r });
                        if sense {
                            self.emit(I::JmpT { c: t, t: 0 })
                        } else {
                            self.emit(I::JmpF { c: t, t: 0 })
                        }
                    }
                    Kind::Val => {
                        if sense {
                            self.emit(I::JmpTV { c: o.r, t: 0 })
                        } else {
                            self.emit(I::JmpFV { c: o.r, t: 0 })
                        }
                    }
                };
                self.reset(m);
                vec![j]
            }
        }
    }

    // ───────────── expressions ─────────────

    /// Evaluate for side effects only.
    fn expr_discard(&mut self, e: &HExpr) {
        let m = self.mark();
        match &e.kind {
            HK::Update { place, inc, .. } => {
                if let Place::Var(v) = &**place
                    && let Home::S(r) = self.home(*v)
                    && self.var_info(*v).kind() == Kind::Num
                {
                    self.emit(I::AddNK {
                        d: r,
                        a: r,
                        k: if *inc { 1.0 } else { -1.0 },
                    });
                } else {
                    self.update(place, *inc, true);
                }
            }
            HK::Call(c) => {
                self.call(c, None);
            }
            HK::Assign(place, value) => {
                // Assign straight into a local's register.
                if let Place::Var(v) = &**place
                    && let Home::S(r) | Home::V(r) = self.home(*v)
                {
                    let k = self.var_info(*v).kind();
                    self.expr_into(value, Op { k, r });
                } else {
                    self.expr(e);
                }
            }
            HK::Seq(es) => {
                for x in es {
                    self.expr_discard(x);
                }
            }
            _ => {
                self.expr(e);
            }
        }
        self.reset(m);
    }

    /// Evaluate into a specific register.
    fn expr_into(&mut self, e: &HExpr, d: Op) {
        // Arithmetic directly into the destination saves a move.
        if let HK::Binary(HBin::Num(op), a, b) = &e.kind
            && d.k == Kind::Num
        {
            let m = self.mark();
            if let HK::Num(k) = b.kind
                && matches!(op, NumOp::Add | NumOp::Sub)
            {
                let x = self.expr(a);
                self.emit(I::AddNK {
                    d: d.r,
                    a: x.r,
                    k: if *op == NumOp::Add { k } else { -k },
                });
            } else {
                let x = self.expr(a);
                let y = self.expr(b);
                self.num_op(*op, d.r, x.r, y.r);
            }
            self.reset(m);
            return;
        }
        let m = self.mark();
        let o = self.expr(e);
        let o = self.coerce(o, d.k);
        self.mov(d, o);
        self.reset(m);
    }

    fn num_op(&mut self, op: NumOp, d: R, a: R, b: R) {
        self.emit(match op {
            NumOp::Add => I::AddN { d, a, b },
            NumOp::Sub => I::SubN { d, a, b },
            NumOp::Mul => I::MulN { d, a, b },
            NumOp::Div => I::DivN { d, a, b },
            op => I::NumOp { op, d, a, b },
        });
    }

    /// Evaluate; the result has kind `e.kind()`. May return a variable's own register.
    fn expr(&mut self, e: &HExpr) -> Op {
        let k = e.kind();
        let o = self.expr_raw(e);
        self.coerce(o, k)
    }

    fn expr_raw(&mut self, e: &HExpr) -> Op {
        let k = e.kind();
        match &e.kind {
            HK::Num(v) => {
                let d = self.alloc_s();
                self.emit(I::LoadN { d, v: *v });
                Op { k: Kind::Num, r: d }
            }
            HK::Bool(v) => {
                let d = self.alloc_s();
                self.emit(I::LoadB { d, v: *v });
                Op { k: Kind::Bool, r: d }
            }
            HK::Str(s) => {
                let d = self.alloc_v();
                let kk = self.str_const(s);
                self.emit(I::LoadK { d, k: kk });
                Op { k: Kind::Val, r: d }
            }
            HK::Null => {
                let d = self.alloc_v();
                self.emit(I::LoadNull { d });
                Op { k: Kind::Val, r: d }
            }
            HK::Undefined => {
                let d = self.alloc_v();
                self.emit(I::LoadUndef { d });
                Op { k: Kind::Val, r: d }
            }
            HK::Read(p) => {
                let pr = self.eval_place(p);
                self.load_place(&pr, k)
            }
            HK::Closure(f) => self.closure(*f),
            HK::NativeFn(n) => {
                let d = self.alloc_v();
                let kk = self.konst(Value::Native(*n));
                self.emit(I::LoadK { d, k: kk });
                Op { k: Kind::Val, r: d }
            }
            HK::Delete(o, key) => {
                let o = self.expr(o);
                let o = self.coerce(o, Kind::Val);
                let kk = self.expr(key);
                let kk = self.coerce(kk, Kind::Val);
                self.emit(I::DelDyn { o: o.r, k: kk.r });
                let d = self.alloc_s();
                self.emit(I::LoadB { d, v: true });
                Op { k: Kind::Bool, r: d }
            }
            HK::ClassRef(c) => {
                let d = self.alloc_v();
                self.emit(I::LoadClass { d, class: *c });
                Op { k: Kind::Val, r: d }
            }
            HK::Array(elems) => {
                let num = matches!(&e.ty, Type::Array(t) if t.kind() == Kind::Num);
                let d = self.alloc_v();
                self.emit(I::NewArr { d, num });
                for el in elems {
                    let m = self.mark();
                    match el {
                        HElem::Expr(x) => {
                            let o = self.expr(x);
                            if num {
                                let o = self.coerce(o, Kind::Num);
                                self.emit(I::PushN { a: d, s: o.r });
                            } else {
                                let o = self.coerce(o, Kind::Val);
                                self.emit(I::PushV { a: d, s: o.r });
                            }
                        }
                        HElem::Spread(x) => {
                            let o = self.expr(x);
                            let o = self.coerce(o, Kind::Val);
                            self.emit(I::PushSpread { a: d, s: o.r });
                        }
                    }
                    self.reset(m);
                }
                Op { k: Kind::Val, r: d }
            }
            HK::Object(props) => {
                let d = self.alloc_v();
                let simple = props.iter().all(|p| matches!(p, HProp::Init(..)));
                if simple {
                    let mut keys: Vec<Rc<str>> = Vec::new();
                    for p in props {
                        if let HProp::Init(n, _) = p
                            && !keys.contains(n)
                        {
                            keys.push(n.clone());
                        }
                    }
                    let shape = self.cg.shape(keys.clone());
                    self.emit(I::NewObj { d, shape });
                    for p in props {
                        let HProp::Init(n, x) = p else { unreachable!() };
                        let slot = keys.iter().position(|k| k == n).unwrap() as u16;
                        let m = self.mark();
                        let o = self.expr(x);
                        if o.k == Kind::Num {
                            self.emit(I::SetFieldN { o: d, i: slot, s: o.r });
                        } else {
                            let o = self.coerce(o, Kind::Val);
                            self.emit(I::SetField { o: d, i: slot, s: o.r });
                        }
                        self.reset(m);
                    }
                } else {
                    let shape = self.cg.shape(vec![]);
                    self.emit(I::NewObj { d, shape });
                    for p in props {
                        let m = self.mark();
                        match p {
                            HProp::Init(n, x) => {
                                let o = self.expr(x);
                                let o = self.coerce(o, Kind::Val);
                                let ps = self.prop_site(n);
                                self.emit(I::SetProp { o: d, p: ps, s: o.r });
                            }
                            HProp::Spread(x) => {
                                let o = self.expr(x);
                                let o = self.coerce(o, Kind::Val);
                                self.emit(I::ObjSpread { o: d, s: o.r });
                            }
                        }
                        self.reset(m);
                    }
                }
                Op { k: Kind::Val, r: d }
            }
            HK::Unary(op, x) => self.unary(*op, x, k),
            HK::Binary(op, a, b) => self.binary(*op, a, b),
            HK::Logical(op, a, b) => {
                // Test the left operand in its own kind, then convert whichever side is used.
                let d = self.alloc(k);
                let av = self.expr(a);
                let use_a = match op {
                    LogicalOp::Nullish => {
                        if av.k == Kind::Val {
                            Some(self.emit(I::JmpNotNullish { s: av.r, t: 0 }))
                        } else {
                            None
                        }
                    }
                    LogicalOp::And | LogicalOp::Or => {
                        // `a && b` uses a when a is falsy; `a || b` when truthy.
                        let sense = *op == LogicalOp::Or;
                        Some(match av.k {
                            Kind::Bool => {
                                if sense {
                                    self.emit(I::JmpT { c: av.r, t: 0 })
                                } else {
                                    self.emit(I::JmpF { c: av.r, t: 0 })
                                }
                            }
                            Kind::Num => {
                                let t = self.alloc_s();
                                self.emit(I::TruthyN { d: t, s: av.r });
                                if sense {
                                    self.emit(I::JmpT { c: t, t: 0 })
                                } else {
                                    self.emit(I::JmpF { c: t, t: 0 })
                                }
                            }
                            Kind::Val => {
                                if sense {
                                    self.emit(I::JmpTV { c: av.r, t: 0 })
                                } else {
                                    self.emit(I::JmpFV { c: av.r, t: 0 })
                                }
                            }
                        })
                    }
                };
                let Some(use_a) = use_a else {
                    // Scalar left side of `??` is never nullish.
                    let av = self.coerce(av, d.k);
                    self.mov(d, av);
                    return d;
                };
                self.expr_into(b, d);
                let end = self.jump_placeholder();
                self.patch_here(use_a);
                let av = self.coerce(av, d.k);
                self.mov(d, av);
                self.patch_here(end);
                d
            }
            HK::Cond(c, a, b) => {
                let d = self.alloc(k);
                let else_j = self.cond_jump(c, false);
                self.expr_into(a, d);
                let end = self.jump_placeholder();
                self.patch_all_here(else_j);
                self.expr_into(b, d);
                self.patch_here(end);
                d
            }
            HK::Assign(place, value) => {
                let pr = self.eval_place(place);
                let v = self.expr(value);
                // Keep the value in a register the store won't clobber.
                let v = if matches!(pr, PlaceRef::Var(_)) {
                    let t = self.alloc(v.k);
                    self.mov(t, v);
                    t
                } else {
                    v
                };
                self.store_place(&pr, v);
                v
            }
            HK::Compound(place, op, value) => {
                let pr = self.eval_place(place);
                let operand_kind = match op {
                    HBin::Num(_) => Kind::Num,
                    _ => Kind::Val,
                };
                let cur = self.load_place(&pr, operand_kind);
                let cur = self.coerce(cur, operand_kind);
                let v = self.expr(value);
                let res = self.binop_regs(*op, cur, v);
                self.store_place(&pr, res);
                res
            }
            HK::LogicalAssign(place, op, value) => {
                let pr = self.eval_place(place);
                let d = self.alloc(k);
                let cur = self.load_place(&pr, k);
                let cur = self.coerce(cur, k);
                self.mov(d, cur);
                let skip = match (op, d.k) {
                    (LogicalOp::Nullish, Kind::Val) => self.emit(I::JmpNotNullish { s: d.r, t: 0 }),
                    (LogicalOp::Nullish, _) => self.jump_placeholder(),
                    (op, Kind::Val) => {
                        if *op == LogicalOp::Or {
                            self.emit(I::JmpTV { c: d.r, t: 0 })
                        } else {
                            self.emit(I::JmpFV { c: d.r, t: 0 })
                        }
                    }
                    (op, Kind::Bool) => {
                        if *op == LogicalOp::Or {
                            self.emit(I::JmpT { c: d.r, t: 0 })
                        } else {
                            self.emit(I::JmpF { c: d.r, t: 0 })
                        }
                    }
                    (op, Kind::Num) => {
                        let t = self.alloc_s();
                        self.emit(I::TruthyN { d: t, s: d.r });
                        if *op == LogicalOp::Or {
                            self.emit(I::JmpT { c: t, t: 0 })
                        } else {
                            self.emit(I::JmpF { c: t, t: 0 })
                        }
                    }
                };
                self.expr_into(value, d);
                self.store_place(&pr, d);
                self.patch_here(skip);
                d
            }
            HK::Update { place, inc, prefix } => self.update(place, *inc, *prefix),
            HK::Call(c) => {
                let d = self.alloc(k);
                self.call(c, Some(d));
                d
            }
            HK::New { class, args } => {
                let d = self.alloc_v();
                self.emit(I::NewInst { d, class: *class });
                let ctor = self.cg.hir.classes[*class as usize].ctor;
                let call = HCall {
                    callee: Callee::Direct { func: ctor, closure: None },
                    this: None,
                    args: args.clone(),
                    param_kinds: None,
                    rest_at: None,
                    ret_kind: Kind::Val,
                    opt: false,
                };
                self.call_with_this(&call, Some(d), None);
                Op { k: Kind::Val, r: d }
            }
            HK::NewBuiltin(n, args) => {
                let d = self.alloc_v();
                let call = HCall {
                    callee: Callee::Native(*n),
                    this: None,
                    args: args.clone(),
                    param_kinds: None,
                    rest_at: None,
                    ret_kind: Kind::Val,
                    opt: false,
                };
                self.call(&call, Some(Op { k: Kind::Val, r: d }));
                Op { k: Kind::Val, r: d }
            }
            HK::InstanceOf(x, c) => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Val);
                let d = self.alloc_s();
                self.emit(I::InstanceOf { d, o: o.r, class: *c });
                Op { k: Kind::Bool, r: d }
            }
            HK::Cast(x) => self.expr(x),
            HK::OptChain(x) => {
                let d = self.alloc(k);
                self.opt_exits.push(Vec::new());
                self.expr_into(x, d);
                let exits = self.opt_exits.pop().unwrap();
                if exits.is_empty() {
                    return d;
                }
                let end = self.jump_placeholder();
                for j in exits {
                    self.patch_here(j);
                }
                match d.k {
                    Kind::Val => self.emit(I::LoadUndef { d: d.r }),
                    Kind::Num => self.emit(I::LoadN { d: d.r, v: f64::NAN }),
                    Kind::Bool => self.emit(I::LoadB { d: d.r, v: false }),
                };
                self.patch_here(end);
                d
            }
            HK::Seq(es) => {
                let (last, init) = es.split_last().unwrap();
                for x in init {
                    self.expr_discard(x);
                }
                self.expr(last)
            }
        }
    }

    fn closure(&mut self, f: FuncId) -> Op {
        let free = self.cg.hir.funcs[f as usize].free.clone();
        let list: Vec<CapSrc> = free
            .iter()
            .map(|v| match self.home(*v) {
                Home::Cell(r) => CapSrc::Local(r),
                Home::Up(i) => CapSrc::Up(i),
                h => panic!(
                    "codegen: captured variable {} has non-cell home {h:?} in {}",
                    self.var_info(*v).name,
                    self.cg.hir.funcs[self.fid as usize].name
                ),
            })
            .collect();
        self.caps.push(list);
        let caps = self.caps.len() as u32 - 1;
        let d = self.alloc_v();
        self.emit(I::MakeClosure { d, f, caps });
        Op { k: Kind::Val, r: d }
    }

    fn unary(&mut self, op: HUnary, x: &HExpr, k: Kind) -> Op {
        match op {
            HUnary::NegNum => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Num);
                let d = self.alloc_s();
                self.emit(I::NegN { d, s: o.r });
                Op { k: Kind::Num, r: d }
            }
            HUnary::BitNotNum => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Num);
                let d = self.alloc_s();
                self.emit(I::BitNotN { d, s: o.r });
                Op { k: Kind::Num, r: d }
            }
            HUnary::NotBool | HUnary::Truthy => {
                let o = self.expr(x);
                let t = match o.k {
                    Kind::Bool => o,
                    Kind::Num => {
                        let d = self.alloc_s();
                        self.emit(I::TruthyN { d, s: o.r });
                        Op { k: Kind::Bool, r: d }
                    }
                    Kind::Val => {
                        let d = self.alloc_s();
                        self.emit(I::TruthyV { d, s: o.r });
                        Op { k: Kind::Bool, r: d }
                    }
                };
                if op == HUnary::Truthy {
                    return t;
                }
                let d = self.alloc_s();
                self.emit(I::NotB { d, s: t.r });
                Op { k: Kind::Bool, r: d }
            }
            HUnary::TypeOf => {
                let o = self.expr(x);
                let d = self.alloc_v();
                match o.k {
                    Kind::Num => {
                        let kk = self.str_const(&"number".into());
                        self.emit(I::LoadK { d, k: kk });
                    }
                    Kind::Bool => {
                        let kk = self.str_const(&"boolean".into());
                        self.emit(I::LoadK { d, k: kk });
                    }
                    Kind::Val => {
                        self.emit(I::TypeOf { d, s: o.r });
                    }
                }
                Op { k: Kind::Val, r: d }
            }
            HUnary::Void => {
                self.expr_discard(x);
                let d = self.alloc_v();
                self.emit(I::LoadUndef { d });
                Op { k: Kind::Val, r: d }
            }
            HUnary::ToNum => {
                let o = self.expr(x);
                match o.k {
                    Kind::Num => o,
                    _ => {
                        let o = self.coerce(o, Kind::Val);
                        let d = self.alloc_s();
                        self.emit(I::ToNum { d, s: o.r });
                        Op { k: Kind::Num, r: d }
                    }
                }
            }
            HUnary::ToStr => {
                let o = self.expr(x);
                let d = self.alloc_v();
                match o.k {
                    Kind::Num => {
                        self.emit(I::NumToStr { d, s: o.r });
                    }
                    _ => {
                        let o = self.coerce(o, Kind::Val);
                        if matches!(x.ty, Type::String | Type::StrLit(_)) {
                            return o;
                        }
                        self.emit(I::ToStr { d, s: o.r });
                    }
                }
                let _ = k;
                Op { k: Kind::Val, r: d }
            }
        }
    }

    fn binary(&mut self, op: HBin, a: &HExpr, b: &HExpr) -> Op {
        // `x + 1` → AddNK
        if let HBin::Num(nop @ (NumOp::Add | NumOp::Sub)) = op
            && let HK::Num(kv) = b.kind
        {
            let x = self.expr(a);
            let x = self.coerce(x, Kind::Num);
            let d = self.alloc_s();
            self.emit(I::AddNK {
                d,
                a: x.r,
                k: if nop == NumOp::Add { kv } else { -kv },
            });
            return Op { k: Kind::Num, r: d };
        }
        let x = self.expr(a);
        let y = self.expr(b);
        self.binop_regs(op, x, y)
    }

    fn binop_regs(&mut self, op: HBin, x: Op, y: Op) -> Op {
        match op {
            HBin::Num(nop) => {
                let x = self.coerce(x, Kind::Num);
                let y = self.coerce(y, Kind::Num);
                let d = self.alloc_s();
                self.num_op(nop, d, x.r, y.r);
                Op { k: Kind::Num, r: d }
            }
            HBin::NumCmp(c) => {
                let x = self.coerce(x, Kind::Num);
                let y = self.coerce(y, Kind::Num);
                let d = self.alloc_s();
                self.emit(match c {
                    CmpOp::Lt => I::LtN { d, a: x.r, b: y.r },
                    CmpOp::Le => I::LeN { d, a: x.r, b: y.r },
                    CmpOp::Gt => I::GtN { d, a: x.r, b: y.r },
                    CmpOp::Ge => I::GeN { d, a: x.r, b: y.r },
                    CmpOp::Eq => I::EqN { d, a: x.r, b: y.r },
                    CmpOp::Ne => I::NeN { d, a: x.r, b: y.r },
                });
                Op { k: Kind::Bool, r: d }
            }
            HBin::BoolEq(eq) => {
                let x = self.coerce(x, Kind::Bool);
                let y = self.coerce(y, Kind::Bool);
                let d = self.alloc_s();
                self.emit(if eq {
                    I::EqB { d, a: x.r, b: y.r }
                } else {
                    I::NeB { d, a: x.r, b: y.r }
                });
                Op { k: Kind::Bool, r: d }
            }
            HBin::StrictEq(eq) => {
                let d = self.alloc_s();
                if x.k == Kind::Num && y.k == Kind::Num {
                    self.emit(if eq {
                        I::EqN { d, a: x.r, b: y.r }
                    } else {
                        I::NeN { d, a: x.r, b: y.r }
                    });
                } else if x.k == Kind::Bool && y.k == Kind::Bool {
                    self.emit(if eq {
                        I::EqB { d, a: x.r, b: y.r }
                    } else {
                        I::NeB { d, a: x.r, b: y.r }
                    });
                } else {
                    let x = self.coerce(x, Kind::Val);
                    let y = self.coerce(y, Kind::Val);
                    self.emit(I::StrictEq { d, a: x.r, b: y.r, eq });
                }
                Op { k: Kind::Bool, r: d }
            }
            HBin::LooseEq(eq) => {
                let d = self.alloc_s();
                if x.k == Kind::Num && y.k == Kind::Num {
                    self.emit(if eq {
                        I::EqN { d, a: x.r, b: y.r }
                    } else {
                        I::NeN { d, a: x.r, b: y.r }
                    });
                } else {
                    let x = self.coerce(x, Kind::Val);
                    let y = self.coerce(y, Kind::Val);
                    self.emit(I::LooseEq { d, a: x.r, b: y.r, eq });
                }
                Op { k: Kind::Bool, r: d }
            }
            HBin::Concat => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_v();
                self.emit(I::Concat { d, a: x.r, b: y.r });
                Op { k: Kind::Val, r: d }
            }
            HBin::StrCmp(c) => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_s();
                self.emit(I::StrCmp { op: c, d, a: x.r, b: y.r });
                Op { k: Kind::Bool, r: d }
            }
            HBin::DynAdd => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_v();
                self.emit(I::DynAdd { d, a: x.r, b: y.r });
                Op { k: Kind::Val, r: d }
            }
            HBin::DynNum(nop) => {
                if x.k == Kind::Num && y.k == Kind::Num {
                    let d = self.alloc_s();
                    self.num_op(nop, d, x.r, y.r);
                    return Op { k: Kind::Num, r: d };
                }
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_s();
                self.emit(I::DynNum { op: nop, d, a: x.r, b: y.r });
                Op { k: Kind::Num, r: d }
            }
            HBin::DynCmp(c) => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_s();
                self.emit(I::DynCmp { op: c, d, a: x.r, b: y.r });
                Op { k: Kind::Bool, r: d }
            }
            HBin::In => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = self.alloc_s();
                self.emit(I::In { d, k: x.r, o: y.r });
                Op { k: Kind::Bool, r: d }
            }
        }
    }

    fn update(&mut self, place: &Place, inc: bool, prefix: bool) -> Op {
        let pr = self.eval_place(place);
        let cur = self.load_place(&pr, Kind::Num);
        let cur = match cur.k {
            Kind::Num => cur,
            Kind::Val => {
                let d = self.alloc_s();
                self.emit(I::ToNum { d, s: cur.r });
                Op { k: Kind::Num, r: d }
            }
            Kind::Bool => self.coerce(cur, Kind::Num),
        };
        // Keep the old value (postfix) out of the variable's own register.
        let old = if !prefix {
            let t = self.alloc_s();
            self.emit(I::MovS { d: t, s: cur.r });
            Op { k: Kind::Num, r: t }
        } else {
            cur
        };
        let new = self.alloc_s();
        self.emit(I::AddNK {
            d: new,
            a: old.r,
            k: if inc { 1.0 } else { -1.0 },
        });
        let new = Op { k: Kind::Num, r: new };
        self.store_place(&pr, new);
        if prefix { new } else { old }
    }

    // ───────────── places ─────────────

    fn opt_check(&mut self, o: Op, opt: bool) {
        if opt {
            let j = self.emit(I::JmpNullish { s: o.r, t: 0 });
            self.opt_exits.last_mut().expect("optional link outside OptChain").push(j);
        }
    }

    fn eval_place(&mut self, p: &Place) -> PlaceRef {
        match p {
            Place::Var(v) => PlaceRef::Var(*v),
            Place::Field { obj, slot, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                PlaceRef::Field {
                    o: o.r,
                    slot: *slot as u16,
                    k: Kind::Val,
                }
            }
            Place::Prop { obj, name, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                let ps = self.prop_site(name);
                PlaceRef::Prop { o: o.r, p: ps }
            }
            Place::Index { obj, index, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                let i = self.expr(index);
                let i = self.coerce(i, Kind::Num);
                let elem_num = matches!(&obj.ty, Type::Array(t) if t.kind() == Kind::Num);
                PlaceRef::Index {
                    a: o.r,
                    i: i.r,
                    k: if elem_num { Kind::Num } else { Kind::Val },
                }
            }
            Place::DynIndex { obj, key, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                let kk = self.expr(key);
                let kk = self.coerce(kk, Kind::Val);
                PlaceRef::Dyn { o: o.r, k: kk.r }
            }
            Place::ArrLen { obj, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                PlaceRef::ArrLen { a: o.r }
            }
            Place::StrLen { obj, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                PlaceRef::StrLen { s: o.r }
            }
        }
    }

    /// Load a place's value; `want` hints the preferred kind (result may differ).
    fn load_place(&mut self, p: &PlaceRef, want: Kind) -> Op {
        match *p {
            PlaceRef::Var(v) => {
                let k = self.var_info(v).kind();
                match self.home(v) {
                    Home::S(r) => Op { k, r },
                    Home::V(r) => Op { k: Kind::Val, r },
                    Home::Cell(c) => {
                        let d = self.alloc_v();
                        self.emit(I::CellGet { d, c });
                        Op { k: Kind::Val, r: d }
                    }
                    Home::Up(i) => {
                        let d = self.alloc_v();
                        self.emit(I::UpGet { d, i });
                        Op { k: Kind::Val, r: d }
                    }
                    Home::GS(g) => {
                        let d = self.alloc_s();
                        self.emit(I::GetGS { d, g });
                        Op { k, r: d }
                    }
                    Home::GV(g) => {
                        let d = self.alloc_v();
                        self.emit(I::GetGV { d, g });
                        Op { k: Kind::Val, r: d }
                    }
                }
            }
            PlaceRef::Field { o, slot, .. } => {
                if want == Kind::Num {
                    let d = self.alloc_s();
                    self.emit(I::GetFieldN { d, o, i: slot });
                    Op { k: Kind::Num, r: d }
                } else {
                    let d = self.alloc_v();
                    self.emit(I::GetField { d, o, i: slot });
                    Op { k: Kind::Val, r: d }
                }
            }
            PlaceRef::Prop { o, p } => {
                let d = self.alloc_v();
                self.emit(I::GetProp { d, o, p });
                Op { k: Kind::Val, r: d }
            }
            PlaceRef::Index { a, i, k } => {
                if k == Kind::Num {
                    let d = self.alloc_s();
                    self.emit(I::GetIdxN { d, a, i });
                    Op { k: Kind::Num, r: d }
                } else {
                    let d = self.alloc_v();
                    self.emit(I::GetIdxV { d, a, i });
                    Op { k: Kind::Val, r: d }
                }
            }
            PlaceRef::Dyn { o, k } => {
                let d = self.alloc_v();
                self.emit(I::GetDyn { d, o, k });
                Op { k: Kind::Val, r: d }
            }
            PlaceRef::ArrLen { a } => {
                let d = self.alloc_s();
                self.emit(I::ArrLen { d, a });
                Op { k: Kind::Num, r: d }
            }
            PlaceRef::StrLen { s } => {
                let d = self.alloc_s();
                self.emit(I::StrLen { d, s });
                Op { k: Kind::Num, r: d }
            }
        }
    }

    fn store_var(&mut self, v: VarId, val: Op) {
        let k = self.var_info(v).kind();
        match self.home(v) {
            Home::S(r) => {
                let val = self.coerce(val, k);
                self.mov(Op { k, r }, val);
            }
            Home::V(r) => {
                let val = self.coerce(val, Kind::Val);
                self.mov(Op { k: Kind::Val, r }, val);
            }
            Home::Cell(c) => {
                let val = self.coerce(val, k);
                let val = self.coerce(val, Kind::Val);
                self.emit(I::CellSet { c, s: val.r });
            }
            Home::Up(i) => {
                let val = self.coerce(val, k);
                let val = self.coerce(val, Kind::Val);
                self.emit(I::UpSet { i, s: val.r });
            }
            Home::GS(g) => {
                let val = self.coerce(val, k);
                self.emit(I::SetGS { g, s: val.r });
            }
            Home::GV(g) => {
                let val = self.coerce(val, Kind::Val);
                self.emit(I::SetGV { g, s: val.r });
            }
        }
    }

    fn store_place(&mut self, p: &PlaceRef, val: Op) {
        match *p {
            PlaceRef::Var(v) => self.store_var(v, val),
            PlaceRef::Field { o, slot, .. } => {
                if val.k == Kind::Num {
                    self.emit(I::SetFieldN { o, i: slot, s: val.r });
                } else {
                    let val = self.coerce(val, Kind::Val);
                    self.emit(I::SetField { o, i: slot, s: val.r });
                }
            }
            PlaceRef::Prop { o, p } => {
                let val = self.coerce(val, Kind::Val);
                self.emit(I::SetProp { o, p, s: val.r });
            }
            PlaceRef::Index { a, i, k } => {
                if k == Kind::Num {
                    let val = self.coerce(val, Kind::Num);
                    self.emit(I::SetIdxN { a, i, s: val.r });
                } else {
                    let val = self.coerce(val, Kind::Val);
                    self.emit(I::SetIdxV { a, i, s: val.r });
                }
            }
            PlaceRef::Dyn { o, k } => {
                let val = self.coerce(val, Kind::Val);
                self.emit(I::SetDyn { o, k, s: val.r });
            }
            PlaceRef::ArrLen { a } => {
                let val = self.coerce(val, Kind::Num);
                self.emit(I::SetArrLen { a, s: val.r });
            }
            PlaceRef::StrLen { .. } => {}
        }
    }

    // ───────────── calls ─────────────

    fn call(&mut self, c: &HCall, dst: Option<Op>) {
        let this = c.this.as_ref().map(|t| {
            let o = self.expr(t);
            self.coerce(o, Kind::Val)
        });
        if let Some(t) = this {
            self.opt_check(t, c.opt && !matches!(c.callee, Callee::Value(_)));
        }
        // Intrinsics: compile hot natives to dedicated instructions.
        if let Callee::Native(n) = c.callee
            && self.intrinsic(n, this, c, dst)
        {
            return;
        }
        self.call_with_this(c, this.map(|t| t.r), dst);
    }

    fn intrinsic(&mut self, n: Native, this: Option<Op>, c: &HCall, dst: Option<Op>) -> bool {
        use Native::*;
        let plain: Vec<&HExpr> = c
            .args
            .iter()
            .filter_map(|a| match a {
                HElem::Expr(e) => Some(e),
                HElem::Spread(_) => None,
            })
            .collect();
        if plain.len() != c.args.len() {
            return false;
        }
        let one = matches!(
            n,
            MathFloor | MathCeil | MathRound | MathTrunc | MathSqrt | MathAbs | MathSign | MathSin | MathCos | MathTan
                | MathAsin | MathAcos | MathAtan | MathExp | MathLog | MathLog2 | MathLog10 | MathCbrt | MathFround
                | MathClz32
        );
        let two = matches!(n, MathAtan2 | MathPow | MathImul | MathMin | MathMax | MathHypot);
        if (one && plain.len() == 1) || (two && plain.len() == 2) {
            if plain.iter().any(|e| e.kind() != Kind::Num) {
                return false;
            }
            let a = self.expr(plain[0]);
            let res = self.alloc_s();
            if one {
                self.emit(I::Math1 { op: n, d: res, s: a.r });
            } else {
                let b = self.expr(plain[1]);
                self.emit(I::Math2 { op: n, d: res, a: a.r, b: b.r });
            }
            if let Some(d) = dst {
                let r = self.coerce(Op { k: Kind::Num, r: res }, d.k);
                self.mov(d, r);
            }
            return true;
        }
        match (n, this) {
            (ArrPush, Some(t)) if plain.len() == 1 && dst.is_none() => {
                let num_arr = matches!(&c.this.as_ref().unwrap().ty, Type::Array(e) if e.kind() == Kind::Num);
                let v = self.expr(plain[0]);
                if num_arr && v.k == Kind::Num {
                    self.emit(I::PushN { a: t.r, s: v.r });
                } else {
                    let v = self.coerce(v, Kind::Val);
                    self.emit(I::PushV { a: t.r, s: v.r });
                }
                true
            }
            (StrCharCodeAt, Some(t)) if plain.len() == 1 && plain[0].kind() == Kind::Num => {
                let i = self.expr(plain[0]);
                let res = self.alloc_s();
                self.emit(I::CharCodeAt { d: res, s: t.r, i: i.r });
                if let Some(d) = dst {
                    let r = self.coerce(Op { k: Kind::Num, r: res }, d.k);
                    self.mov(d, r);
                }
                true
            }
            _ => false,
        }
    }

    fn call_with_this(&mut self, c: &HCall, this: Option<R>, dst: Option<Op>) {
        let (target, param_kinds, rest, pack) = match &c.callee {
            Callee::Direct { func, closure } => {
                let hf = &self.cg.hir.funcs[*func as usize];
                let kinds: Vec<Kind> = hf.params.iter().map(|p| p.arrival).collect();
                let rest = hf.params.last().is_some_and(|p| p.rest);
                let t = match closure.filter(|_| !hf.free.is_empty()) {
                    Some(v) => {
                        let o = self.load_place(&PlaceRef::Var(v), Kind::Val);
                        let o = self.coerce(o, Kind::Val);
                        CallTarget::Value(o.r)
                    }
                    None => CallTarget::Direct(*func),
                };
                (t, Some(kinds), rest, true)
            }
            Callee::Value(f) => {
                let o = self.expr(f);
                let o = self.coerce(o, Kind::Val);
                if c.opt {
                    self.opt_check(o, true);
                }
                (CallTarget::Value(o.r), c.param_kinds.clone(), false, false)
            }
            Callee::Method { vslot, .. } => (CallTarget::Method(*vslot), c.param_kinds.clone(), c.rest_at.is_some(), true),
            Callee::Prop(name) => {
                let p = self.prop_site(name);
                (CallTarget::Prop(p), c.param_kinds.clone(), false, false)
            }
            Callee::Native(n) => (CallTarget::Native(*n), None, false, false),
        };
        let mut args = Vec::new();
        let mut spread = Vec::new();
        let rest_at = if pack && rest {
            param_kinds.as_ref().map(|k| k.len() - 1)
        } else {
            None
        };
        let mut i = 0;
        let mut rest_arr: Option<R> = None;
        for a in &c.args {
            if let Some(ra) = rest_at
                && i >= ra
            {
                let arr = *rest_arr.get_or_insert_with(|| {
                    let d = self.alloc_v();
                    self.code.push(I::NewArr { d, num: false });
                    d
                });
                match a {
                    HElem::Expr(e) => {
                        let o = self.expr(e);
                        let o = self.coerce(o, Kind::Val);
                        self.emit(I::PushV { a: arr, s: o.r });
                    }
                    HElem::Spread(e) => {
                        let o = self.expr(e);
                        let o = self.coerce(o, Kind::Val);
                        self.emit(I::PushSpread { a: arr, s: o.r });
                    }
                }
                i += 1;
                continue;
            }
            match a {
                HElem::Expr(e) => {
                    let o = self.expr(e);
                    let k = param_kinds.as_ref().and_then(|ks| ks.get(i).copied()).unwrap_or(o.k);
                    let o = self.coerce(o, k);
                    args.push((o.k, o.r));
                    spread.push(false);
                }
                HElem::Spread(e) => {
                    let o = self.expr(e);
                    let o = self.coerce(o, Kind::Val);
                    args.push((Kind::Val, o.r));
                    spread.push(true);
                }
            }
            i += 1;
        }
        if let Some(ra) = rest_at {
            // Fill skipped optional params before the rest param, then the rest array.
            while args.len() < ra {
                let k = param_kinds.as_ref().unwrap()[args.len()];
                let o = self.alloc(k);
                match k {
                    Kind::Val => self.emit(I::LoadUndef { d: o.r }),
                    Kind::Num => self.emit(I::LoadN { d: o.r, v: f64::NAN }),
                    Kind::Bool => self.emit(I::LoadB { d: o.r, v: false }),
                };
                args.push((k, o.r));
                spread.push(false);
            }
            let arr = rest_arr.unwrap_or_else(|| {
                let d = self.alloc_v();
                self.emit(I::NewArr { d, num: false });
                d
            });
            args.push((Kind::Val, arr));
            spread.push(false);
        }
        let ci = CallInfo {
            target,
            this,
            args,
            spread,
            dst: dst.map(|d| (d.k, d.r)),
            packed_rest: rest_at.is_some(),
        };
        self.calls.push(ci);
        let ci = self.calls.len() as u32 - 1;
        self.emit(I::Call { ci });
    }
}
