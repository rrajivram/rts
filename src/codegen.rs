//! HIR → instructions. Gives every variable and temporary a typed slot
//! (number/boolean slots or value slots, see `bytecode.rs`) and inserts
//! box/unbox instructions wherever a value moves between slot types.

use std::collections::HashMap;
use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::{LogicalOp, Name};
use crate::bytecode::*;
use crate::hir::*;
use crate::runtime::{ClassRt, Native, PropCache, Shape, Value};
use crate::types::{Kind, Type};

/// An evaluated operand: the type of slot it is in (`k`) and the slot's index
/// (`r`) in that slot array (scalar slots for `Num`/`Bool`, value slots for `Val`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Op {
    k: Kind,
    r: u16,
}

/// Where a variable lives.
#[derive(Debug, Clone, Copy)]
enum Home {
    /// A number/boolean slot of the frame.
    Scalar(u16),
    /// A value slot of the frame.
    Value(u16),
    /// A value slot holding the variable's cell (the variable is captured by a closure).
    Cell(u16),
    /// A cell the running closure captured.
    Capture(u16),
    /// A global number/boolean variable.
    GlobalScalar(u32),
    /// A global value variable.
    GlobalValue(u32),
}

/// An evaluated assignment target.
enum PlaceRef {
    Var(VarId),
    /// `name` is the field's name, for `--dump`.
    Field { o: u16, slot: u16, name: Option<Rc<str>> },
    Prop { o: u16, p: u32 },
    Index { a: u16, i: u16, k: Kind },
    Dyn { o: u16, k: u16 },
    ArrLen { a: u16 },
    StrLen { s: u16 },
    Accessor { o: u16, class: u32, get: Option<u32>, set: Option<u32> },
}

struct LoopCtx {
    label: Option<Name>,
    is_switch: bool,
    breaks: Vec<usize>,
    continues: Vec<usize>,
    try_depth: usize,
    /// Regions active when the loop started (left by `break`/`continue`).
    region_depth: usize,
}

struct TryCtx {
    finalizer: Option<Vec<HStmt>>,
    /// Has an active TryBegin handler that must be popped when jumping out.
    has_handler: bool,
    /// A `for-of` iterator (in this value slot) to close when jumping out.
    close_iterator: Option<u16>,
}

pub fn compile(hir: &HProgram) -> Program {
    let mut cg = Codegen {
        hir,
        escape: crate::escape::analyze(hir),
        shapes: Vec::new(),
        shape_ids: HashMap::new(),
        globals: HashMap::new(),
        n_gs: 0,
        n_gv: 0,
    };
    for (i, v) in hir.vars.iter().enumerate() {
        if v.owner.is_none() {
            let h = if v.kind().is_scalar() {
                cg.n_gs += v.kind().width() as usize;
                Home::GlobalScalar((cg.n_gs - v.kind().width() as usize) as u32)
            } else {
                cg.n_gv += 1;
                Home::GlobalValue(cg.n_gv as u32 - 1)
            };
            cg.globals.insert(i as VarId, h);
        }
    }
    let funcs: Vec<Rc<FunctionCode>> = (0..hir.funcs.len()).map(|f| Rc::new(cg.func(f as FuncId))).collect();
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
            acyclic: hc.acyclic,
        });
        out[i] = Some(c.clone());
        c
    }
    for i in 0..hir.classes.len() {
        build(i, hir, &mut classes);
    }
    let global_scalar_names = cg.global_names(false);
    let global_value_names = cg.global_names(true);
    Program {
        funcs,
        classes: classes.into_iter().map(|c| c.unwrap()).collect(),
        shapes: cg.shapes,
        global_scalars: cg.n_gs,
        global_values: cg.n_gv,
        global_scalar_names,
        global_value_names,
        main: hir.main,
        error_class: hir.error_class,
        type_error_class: hir.type_error_class,
        range_error_class: hir.range_error_class,
        syntax_error_class: hir.syntax_error_class,
    }
}

struct Codegen<'a> {
    hir: &'a HProgram,
    /// Which allocations are scope-bound, and which blocks hold regions.
    escape: crate::escape::EscapeInfo,
    shapes: Vec<Rc<Shape>>,
    shape_ids: HashMap<Vec<Rc<str>>, u32>,
    globals: HashMap<VarId, Home>,
    n_gs: usize,
    n_gv: usize,
}

impl<'a> Codegen<'a> {
    /// Names of the global value variables (`values`) or scalar variables, by
    /// index (a bigint's second slot has no name of its own).
    fn global_names(&self, values: bool) -> Vec<Rc<str>> {
        let mut out: Vec<Rc<str>> = vec![Rc::from(""); if values { self.n_gv } else { self.n_gs }];
        for (v, h) in &self.globals {
            match (h, values) {
                (Home::GlobalValue(g), true) | (Home::GlobalScalar(g), false) => out[*g as usize] = self.hir.vars[*v as usize].name.clone(),
                _ => {}
            }
        }
        out
    }

    fn shape(&mut self, keys: Vec<Rc<str>>) -> u32 {
        if let Some(i) = self.shape_ids.get(&keys) {
            return *i;
        }
        let i = self.shapes.len() as u32;
        self.shapes.push(Shape::new(keys.clone()));
        self.shape_ids.insert(keys, i);
        i
    }

    fn func(&mut self, fid: FuncId) -> FunctionCode {
        let hir: &'a HProgram = self.hir;
        let hf = &hir.funcs[fid as usize];
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
            num_regs: HashMap::new(),
            inline: Vec::new(),
            slot_names: vec![SlotName { kind: Kind::Val, index: 0, name: Some("this".into()), from: 0 }],
            field_names: Vec::new(),
            regions: Vec::new(),
            peak_v: 0,
            pending_regions: Vec::new(),
        };
        // Arrival slots: value slot 0 = this, then each parameter in a slot of its type.
        let mut arrivals = Vec::new();
        for p in &hf.params {
            let r = g.alloc(p.arrival).r;
            g.note_slot(p.arrival, r, Some(hir.vars[p.var as usize].name.clone()));
            arrivals.push(r);
        }
        // Borrowed slots: `this` and every value parameter whose arrival slot
        // is never written again (no default, not rest, and either captured,
        // so it moves to a cell, or never assigned). Coroutines keep their
        // frames past the call, so they borrow nothing.
        let mut borrowed: u64 = 0;
        if !hf.is_async && !hf.is_generator {
            borrowed |= 1;
            for (p, &r) in hf.params.iter().zip(&arrivals) {
                let info = &hir.vars[p.var as usize];
                if p.arrival == Kind::Val
                    && p.default.is_none()
                    && !p.rest
                    && r < 64
                    && (info.captured || !g.cg.escape.assigned.contains(&p.var))
                {
                    borrowed |= 1 << r;
                }
            }
        }
        // Constant slots: every distinct numeric literal used in a loop is loaded once.
        let mut nums = Vec::new();
        for p in &hf.params {
            if let Some(d) = &p.default {
                collect_nums_expr(d, &mut nums, hir);
            }
        }
        for st in &hf.body {
            collect_loop_nums(st, &mut nums, hir);
        }
        for v in nums.into_iter().take(128) {
            let bits = v.to_bits();
            if g.num_regs.contains_key(&bits) {
                continue;
            }
            let r = g.alloc_s();
            g.note_slot(Kind::Num, r, Some(format!("#{}", crate::runtime::num_to_string(v)).into()));
            g.emit(Instruction::LoadNumber { output: NumSlot(r), value: v });
            g.num_regs.insert(bits, r);
        }
        for (i, v) in hf.free.iter().enumerate() {
            g.homes.insert(*v, Home::Capture(i as u16));
        }
        if let Some(tv) = hf.this_var {
            g.bind_arrival(tv, Kind::Val, 0);
        }
        for (p, r) in hf.params.iter().zip(arrivals) {
            if let Some(d) = &p.default {
                let skip = g.jump_placeholder();
                g.code[skip] = Instruction::JumpIfNullish { when: false, value: ValSlot(r), to: 0 };
                let m = g.mark();
                let dv = g.expr(d);
                let dv = g.coerce(dv, Kind::Val);
                g.emit(Instruction::CopyValue { to: ValSlot(r), from: ValSlot(dv.r) });
                g.reset(m);
                g.patch_here(skip);
            }
            g.bind_arrival(p.var, p.arrival, r);
        }
        if hf.is_generator {
            g.emit(Instruction::CreateGenerator);
        } else if hf.is_async {
            g.emit(Instruction::StartAsync);
        }
        for st in &hf.body {
            g.stmt(st);
        }
        g.emit(Instruction::ReturnUndefined);
        if let Some(new_index) = peephole(&mut g.code) {
            for n in &mut g.slot_names {
                n.from = new_index[n.from as usize];
            }
            for (pc, _) in &mut g.field_names {
                *pc = new_index[*pc as usize];
            }
        }
        let name = hf.name.clone();
        FunctionCode {
            id: fid,
            jit: None,
            borrowed,
            is_async: hf.is_async,
            name,
            code: g.code,
            scalar_slots: g.max_s as usize,
            value_slots: g.max_v as usize,
            params: hf.params.iter().map(|p| p.arrival).collect(),
            rest: hf.params.last().is_some_and(|p| p.rest),
            constants: g.consts,
            call_sites: g.calls,
            property_sites: g.props,
            capture_lists: g.caps,
            capture_names: hf.free.iter().map(|v| hir.vars[*v as usize].name.clone()).collect(),
            slot_names: g.slot_names,
            field_names: g.field_names,
            dump_name: match hf.class {
                Some(c) if hf.is_ctor => format!("{} constructor", hir.classes[c as usize].name).into(),
                Some(c) => format!("{}.{}", hir.classes[c as usize].name, hf.name).into(),
                None if hf.name.is_empty() => format!("<anonymous #{fid}>").into(),
                None => hf.name.clone(),
            },
            param_names: hf.params.iter().map(|p| hir.vars[p.var as usize].name.clone()).collect(),
            prelude: hf.class.is_some_and(|c| hir.classes[c as usize].prelude),
        }
    }
}

struct FnGen<'c, 'a> {
    cg: &'c mut Codegen<'a>,
    fid: FuncId,
    code: Vec<Instruction>,
    s_top: u16,
    v_top: u16,
    max_s: u16,
    max_v: u16,
    consts: Vec<Value>,
    str_consts: HashMap<Rc<str>, u32>,
    calls: Vec<CallSite>,
    props: Vec<PropertySite>,
    caps: Vec<Vec<CaptureFrom>>,
    homes: HashMap<VarId, Home>,
    loops: Vec<LoopCtx>,
    tries: Vec<TryCtx>,
    opt_exits: Vec<Vec<usize>>,
    /// Numeric literals preloaded into dedicated slots (f64 bits → slot).
    num_regs: HashMap<u64, u16>,
    /// Functions currently being inlined (innermost last).
    inline: Vec<InlineCtx>,
    /// What each slot holds over time (for `--dump`).
    slot_names: Vec<SlotName>,
    /// Field names of field instructions, by instruction index (for `--dump`).
    field_names: Vec<(u32, Rc<str>)>,
    /// Blocks with an active region, innermost last (keys as in `EscapeInfo`).
    regions: Vec<usize>,
    /// Highest value slot in use since the innermost region started.
    peak_v: u16,
    /// Result regions for calls being emitted, keyed by the `HCall` (see
    /// `CallSite::region`); nested calls in arguments push their own.
    pending_regions: Vec<(usize, Option<u16>)>,
}

struct InlineCtx {
    /// Where `return` stores its value (None: discard).
    out: Option<Op>,
    exits: Vec<usize>,
    try_depth: usize,
    loop_depth: usize,
    region_depth: usize,
}

impl FnGen<'_, '_> {
    // ───────────── slots & emission ─────────────

    fn alloc_s(&mut self) -> u16 {
        let r = self.s_top;
        self.s_top += 1;
        self.max_s = self.max_s.max(self.s_top);
        self.note_slot(Kind::Num, r, None);
        r
    }
    fn alloc_v(&mut self) -> u16 {
        let r = self.v_top;
        self.v_top += 1;
        self.max_v = self.max_v.max(self.v_top);
        self.peak_v = self.peak_v.max(self.v_top);
        self.note_slot(Kind::Val, r, None);
        r
    }
    /// Record that from the next instruction on, a slot holds `name` (`None`: a temporary).
    fn note_slot(&mut self, kind: Kind, index: u16, name: Option<Rc<str>>) {
        let kind = if kind == Kind::Val { Kind::Val } else { Kind::Num };
        let from = self.code.len() as u32;
        if let Some(last) = self.slot_names.iter_mut().rev().find(|n| n.kind == kind && n.index == index)
            && last.from == from
        {
            last.name = name;
            return;
        }
        self.slot_names.push(SlotName { kind, index, name, from });
    }
    /// The next instruction accesses the field `name` (for `--dump`).
    fn note_field(&mut self, name: &Option<Rc<str>>) {
        if let Some(n) = name {
            let pc = self.code.len() as u32;
            self.field_names.push((pc, n.clone()));
        }
    }
    fn with_field_name(&mut self, name: &Option<Rc<str>>, i: Instruction) -> Instruction {
        self.note_field(name);
        i
    }
    fn set_home(&mut self, var: VarId, h: Home) {
        self.homes.insert(var, h);
        self.name_var_slot(var, h);
    }
    fn name_var_slot(&mut self, var: VarId, h: Home) {
        let name = self.var_info(var).name.clone();
        match h {
            Home::Scalar(r) => self.note_slot(Kind::Num, r, Some(name)),
            Home::Value(r) | Home::Cell(r) => self.note_slot(Kind::Val, r, Some(name)),
            _ => {}
        }
    }
    fn alloc(&mut self, k: Kind) -> Op {
        let r = match k {
            Kind::Val => self.alloc_v(),
            Kind::Big => {
                let r = self.alloc_s();
                self.alloc_s();
                r
            }
            _ => self.alloc_s(),
        };
        Op { k, r }
    }
    /// A BigInt constant (for `LoadBigInt`).
    fn big_const(&mut self, n: i128) -> u32 {
        self.konst(Value::bigint(n))
    }
    fn load_big(&mut self, n: i128, output: u16) {
        let constant = self.big_const(n);
        self.emit(Instruction::LoadBigInt { constant, output: BigSlot(output) });
    }
    fn mark(&self) -> (u16, u16) {
        (self.s_top, self.v_top)
    }
    fn reset(&mut self, m: (u16, u16)) {
        self.s_top = m.0;
        self.v_top = m.1;
    }
    fn emit(&mut self, i: Instruction) -> usize {
        self.code.push(i);
        self.code.len() - 1
    }
    fn here(&self) -> u32 {
        self.code.len() as u32
    }
    fn jump_placeholder(&mut self) -> usize {
        self.emit(Instruction::Jump { to: u32::MAX })
    }
    fn patch(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Instruction::Jump { to: t }
            | Instruction::JumpIf { to: t, .. }
            | Instruction::JumpIfCompare { to: t, .. }
            | Instruction::JumpIfTruthy { to: t, .. }
            | Instruction::JumpIfNullish { to: t, .. }
            | Instruction::IteratorStep { to: t, .. }
            | Instruction::UnpackIteratorResult { to: t, .. } => *t = target,
            Instruction::TryBegin { catch, .. } => *catch = target,
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
        self.props.push(PropertySite {
            name: name.clone(),
            key: Value::str(&**name),
            cache: PropCache::default(),
            is_length: &**name == "length",
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

    /// Give a parameter/this variable its home given its arrival slot.
    fn bind_arrival(&mut self, var: VarId, arrival: Kind, r: u16) {
        let info = self.var_info(var).clone();
        if info.captured {
            let c = self.alloc_v();
            self.emit(Instruction::NewCell { output: ValSlot(c) });
            let val = self.coerce(Op { k: arrival, r }, Kind::Val);
            self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(val.r) });
            self.set_home(var, Home::Cell(c));
            return;
        }
        let k = info.kind();
        if k == arrival {
            self.set_home(var, if k == Kind::Val { Home::Value(r) } else { Home::Scalar(r) });
        } else {
            let h = self.alloc(k);
            let src = self.coerce(Op { k: arrival, r }, k);
            self.mov(h, src);
            self.set_home(var, if k == Kind::Val { Home::Value(h.r) } else { Home::Scalar(h.r) });
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
            Home::Value(self.alloc_v())
        } else {
            let k = info.kind();
            Home::Scalar(self.alloc(k).r)
        };
        self.set_home(var, h);
        h
    }

    fn mov(&mut self, d: Op, s: Op) {
        debug_assert_eq!(d.k == Kind::Val, s.k == Kind::Val);
        if d.r == s.r && d.k == s.k {
            return;
        }
        match d.k {
            Kind::Val => self.emit(Instruction::CopyValue { to: ValSlot(d.r), from: ValSlot(s.r) }),
            Kind::Num => self.emit(Instruction::CopyNumber { to: NumSlot(d.r), from: NumSlot(s.r) }),
            Kind::Bool => self.emit(Instruction::CopyBool { to: BoolSlot(d.r), from: BoolSlot(s.r) }),
            Kind::Big => self.emit(Instruction::CopyBigInt { to: BigSlot(d.r), from: BigSlot(s.r) }),
        };
    }

    /// Convert an operand to another kind (boxing / checked unboxing).
    fn coerce(&mut self, o: Op, k: Kind) -> Op {
        if o.k == k {
            return o;
        }
        match (o.k, k) {
            (Kind::Num, Kind::Val) => {
                let d = self.alloc_v();
                self.emit(Instruction::BoxNumber { output: ValSlot(d), value: NumSlot(o.r) });
                Op { k, r: d }
            }
            (Kind::Bool, Kind::Val) => {
                let d = self.alloc_v();
                self.emit(Instruction::BoxBool { output: ValSlot(d), value: BoolSlot(o.r) });
                Op { k, r: d }
            }
            (Kind::Val, Kind::Num) => {
                let d = self.alloc_s();
                self.emit(Instruction::UnboxNumber { output: NumSlot(d), value: ValSlot(o.r) });
                Op { k, r: d }
            }
            (Kind::Val, Kind::Bool) => {
                let d = self.alloc_s();
                self.emit(Instruction::UnboxBool { output: BoolSlot(d), value: ValSlot(o.r) });
                Op { k, r: d }
            }
            (Kind::Big, Kind::Val) => {
                let d = self.alloc_v();
                self.emit(Instruction::BoxBigInt { output: ValSlot(d), value: BigSlot(o.r) });
                Op { k, r: d }
            }
            (Kind::Val, Kind::Big) => {
                let d = self.alloc(Kind::Big);
                self.emit(Instruction::UnboxBigInt { output: BigSlot(d.r), value: ValSlot(o.r) });
                d
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
        // A block with scope-bound allocations gets a region: entered here,
        // left (its slots cleared, its arena freed) when the block ends.
        let region = !ss.is_empty() && self.cg.escape.regions.contains(&(ss.as_ptr() as usize));
        let saved_peak = self.peak_v;
        let enter = if region {
            self.regions.push(ss.as_ptr() as usize);
            self.peak_v = self.v_top;
            Some(self.emit(Instruction::EnterRegion { first: ValSlot(m.1), count: 0 }))
        } else {
            None
        };
        for v in vars {
            if self.cg.globals.contains_key(v) {
                continue;
            }
            self.homes.remove(v);
            if let Home::Cell(c) = self.declare(*v) {
                self.emit(Instruction::NewCell { output: ValSlot(c) });
            }
        }
        self.stmts(ss);
        if let Some(at) = enter {
            let count = self.peak_v.max(self.v_top) - m.1;
            self.code[at] = Instruction::EnterRegion { first: ValSlot(m.1), count };
            self.emit(Instruction::ExitRegion);
            self.regions.pop();
            self.peak_v = saved_peak.max(self.peak_v);
        } else if self.v_top > m.1 {
            self.emit(Instruction::Release { first: ValSlot(m.1), count: self.v_top - m.1 });
        }
        self.reset(m);
    }

    /// Leave regions entered since depth `depth` (before jumping out of them).
    fn exit_regions_to(&mut self, depth: usize) {
        if self.regions.len() > depth {
            self.emit(Instruction::ExitRegionsTo { depth: depth as u16 });
        }
    }

    /// The region (as `up` levels out from the innermost) an allocation goes
    /// in, if the escape analysis bound it to a scope.
    fn region_of(&self, e: &HExpr) -> Option<u16> {
        let key = e as *const HExpr as usize;
        if self.cg.escape.result.contains(&key) && !self.cg.hir.funcs[self.fid as usize].is_async {
            return Some(crate::bytecode::RESULT_REGION);
        }
        let blk = self.cg.escape.sites.get(&key)?;
        let pos = self.regions.iter().rposition(|k| k == blk)?;
        Some((self.regions.len() - 1 - pos) as u16)
    }

    /// Default value for an uninitialized declaration (`let x: number;`).
    fn init_default(&mut self, var: VarId) {
        let k = self.var_info(var).kind();
        let t = self.alloc(k);
        match k {
            Kind::Num => self.emit(Instruction::LoadNumber { output: NumSlot(t.r), value: f64::NAN }),
            Kind::Bool => self.emit(Instruction::LoadBool { output: BoolSlot(t.r), value: false }),
            Kind::Big => {
                self.load_big(0, t.r);
                0
            }
            Kind::Val => self.emit(Instruction::LoadUndefined { output: ValSlot(t.r) }),
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
                self.emit(Instruction::NewCell { output: ValSlot(c) });
                if let Some(v) = val {
                    let v = self.coerce(v, Kind::Val);
                    self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(v.r) });
                } else if k == Kind::Num {
                    let t = self.alloc_v();
                    let kk = self.konst(Value::Num(f64::NAN));
                    self.emit(Instruction::LoadConstant { output: ValSlot(t), constant: kk });
                    self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(t) });
                } else if k.is_scalar() {
                    let t = self.alloc_v();
                    let kk = self.konst(if k == Kind::Big { Value::bigint(0) } else { Value::Bool(false) });
                    self.emit(Instruction::LoadConstant { output: ValSlot(t), constant: kk });
                    self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(t) });
                }
            }
            _ => match val {
                Some(v) => self.store_var(var, v),
                None => match (h, k) {
                    (Home::Scalar(r), Kind::Num) => {
                        self.emit(Instruction::LoadNumber { output: NumSlot(r), value: f64::NAN });
                    }
                    (Home::Scalar(r), Kind::Big) => self.load_big(0, r),
                    (Home::Scalar(r), _) => {
                        self.emit(Instruction::LoadBool { output: BoolSlot(r), value: false });
                    }
                    (Home::Value(r), _) => {
                        self.emit(Instruction::LoadUndefined { output: ValSlot(r) });
                    }
                    (Home::GlobalScalar(_), _) | (Home::GlobalValue(_), _) => {
                        let t = self.alloc(k);
                        match k {
                            Kind::Num => self.emit(Instruction::LoadNumber { output: NumSlot(t.r), value: f64::NAN }),
                            Kind::Bool => self.emit(Instruction::LoadBool { output: BoolSlot(t.r), value: false }),
                            Kind::Big => {
                                self.load_big(0, t.r);
                                0
                            }
                            Kind::Val => self.emit(Instruction::LoadUndefined { output: ValSlot(t.r) }),
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
                        self.emit(Instruction::NewCell { output: ValSlot(c) });
                    }
                }
                let m2 = self.mark();
                let k = self.var_info(*var).kind();
                match (init, self.home(*var)) {
                    (Some(e), Home::Scalar(r) | Home::Value(r)) => self.expr_into(e, Op { k, r }),
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
                        self.emit(Instruction::NewCell { output: ValSlot(c) });
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
                    region_depth: self.regions.len(),
                });
                // Rotated loop: test at the bottom, one branch per iteration.
                let to_test = if *test_after || test.is_none() { None } else { Some(self.jump_placeholder()) };
                let top = self.here();
                self.block(body);
                let cont = self.here();
                // Fresh per-iteration bindings for captured loop variables.
                for v in fresh {
                    if let Some(Home::Cell(c)) = self.homes.get(v).copied() {
                        let t = self.alloc_v();
                        self.emit(Instruction::ReadCell { output: ValSlot(t), cell: ValSlot(c) });
                        self.emit(Instruction::NewCell { output: ValSlot(c) });
                        self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(t) });
                        self.v_top -= 1;
                    }
                }
                if let Some(u) = update {
                    let mm = self.mark();
                    self.expr_discard(u);
                    self.reset(mm);
                }
                if let Some(j) = to_test {
                    self.patch_here(j);
                }
                match test {
                    Some(t) => {
                        let mm = self.mark();
                        let back = self.cond_jump(t, true);
                        self.reset(mm);
                        for j in back {
                            self.patch(j, top);
                        }
                    }
                    None => {
                        self.emit(Instruction::Jump { to: top });
                    }
                }
                let ctx = self.loops.pop().unwrap();
                for c in ctx.continues {
                    self.patch(c, cont);
                }
                for b in ctx.breaks {
                    self.patch_here(b);
                }
            }
            HStmt::ForOf { label, elem, iter, body, is_await: true } => self.for_await(label, *elem, iter, body),
            HStmt::ForOf { label, elem, iter, body, .. } if !matches!(iter.ty.non_null(), Type::Array(_) | Type::Tuple(_) | Type::String | Type::StrLit(_) | Type::Map(..) | Type::Set(_)) => {
                self.for_of_iterator(label, *elem, iter, body)
            }
            HStmt::ForOf { label, elem, iter, body, .. } | HStmt::ForIn {
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
                    self.emit(Instruction::KeysArray { output: ValSlot(arr), object: ValSlot(src.r) });
                } else {
                    self.emit(Instruction::ToIterationArray { output: ValSlot(arr), value: ValSlot(src.r) });
                }
                let idx = self.alloc_s();
                self.emit(Instruction::LoadNumber { output: NumSlot(idx), value: 0.0 });
                let len = self.alloc_s();
                self.loops.push(LoopCtx {
                    label: label.clone(),
                    is_switch: false,
                    breaks: vec![],
                    continues: vec![],
                    try_depth: self.tries.len(),
                    region_depth: self.regions.len(),
                });
                let top = self.here();
                self.emit(Instruction::ArrayLength { output: NumSlot(len), array: ValSlot(arr) });
                let exit = self.emit(Instruction::JumpIfCompare { op: CmpOp::Lt, when: false, left: NumSlot(idx), right: NumSlot(len), to: 0 });
                let bm = self.mark();
                let ek = self.var_info(*elem).kind();
                let el = self.alloc(if ek == Kind::Num { Kind::Num } else { Kind::Val });
                if el.k == Kind::Num {
                    self.emit(Instruction::ReadElementNumber { output: NumSlot(el.r), array: ValSlot(arr), index: NumSlot(idx) });
                } else {
                    self.emit(Instruction::ReadElement { output: ValSlot(el.r), array: ValSlot(arr), index: NumSlot(idx) });
                }
                let el = self.coerce(el, ek);
                self.homes.remove(elem);
                self.init_var(*elem, Some(el));
                self.block(body);
                self.reset(bm);
                let cont = self.here();
                self.emit(Instruction::AddConstant { output: NumSlot(idx), left: NumSlot(idx), constant: 1.0 });
                self.emit(Instruction::Jump { to: top });
                let ctx = self.loops.pop().unwrap();
                for c in ctx.continues {
                    self.patch(c, cont);
                }
                self.patch_here(exit);
                for b in ctx.breaks {
                    self.patch_here(b);
                }
                self.emit(Instruction::LoadUndefined { output: ValSlot(arr) });
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
                let rd = self.loops[i].region_depth;
                self.exit_regions_to(rd);
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
                let rd = self.loops[i].region_depth;
                self.exit_regions_to(rd);
                let j = self.jump_placeholder();
                self.loops[i].continues.push(j);
            }
            HStmt::Return(v) if !self.inline.is_empty() => {
                let out = self.inline.last().unwrap().out;
                if let Some(e) = v {
                    match out {
                        Some(o) => self.expr_into(e, o),
                        None => self.expr_discard(e),
                    }
                } else if let Some(o) = out {
                    self.load_default(o);
                }
                let depth = self.inline.last().unwrap().try_depth;
                self.exit_tries(depth);
                let rd = self.inline.last().unwrap().region_depth;
                self.exit_regions_to(rd);
                let j = self.jump_placeholder();
                self.inline.last_mut().unwrap().exits.push(j);
                self.reset(m);
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
                    None => self.emit(Instruction::ReturnUndefined),
                    Some(o) => match o.k {
                        Kind::Num => self.emit(Instruction::ReturnNumber { value: NumSlot(o.r) }),
                        Kind::Bool => self.emit(Instruction::ReturnBool { value: BoolSlot(o.r) }),
                        Kind::Big => self.emit(Instruction::ReturnBigInt { value: BigSlot(o.r) }),
                        Kind::Val => self.emit(Instruction::ReturnValue { value: ValSlot(o.r) }),
                    },
                };
                self.reset(m);
            }
            HStmt::Throw(e) => {
                let o = self.expr(e);
                let o = self.coerce(o, Kind::Val);
                self.emit(Instruction::Throw { value: ValSlot(o.r) });
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
                    region_depth: self.regions.len(),
                });
                self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                for b in ctx.breaks {
                    self.patch_here(b);
                }
            }
        }
    }

    /// A number slot holding `v` (for comparing a resume mode).
    fn num_const(&mut self, v: f64) -> u16 {
        if let Some(r) = self.num_regs.get(&v.to_bits()) {
            return *r;
        }
        let r = self.alloc_s();
        self.emit(Instruction::LoadNumber { output: NumSlot(r), value: v });
        r
    }

    /// `await` the value in slot `v`: a rejection throws. Result in a value slot.
    fn await_value(&mut self, v: u16) -> Op {
        let received = self.alloc_v();
        let mode = self.alloc_s();
        self.emit(Instruction::Await { value: ValSlot(v), received: ValSlot(received), mode: NumSlot(mode) });
        let zero = self.num_const(0.0);
        let j = self.emit(Instruction::JumpIfCompare { op: CmpOp::Eq, when: true, left: NumSlot(mode), right: NumSlot(zero), to: 0 });
        self.emit(Instruction::Throw { value: ValSlot(received) });
        self.patch_here(j);
        Op { k: Kind::Val, r: received }
    }

    /// Return the value in `o` from the function, running finalizers.
    fn return_op(&mut self, o: Op) {
        let t = self.alloc(Kind::Val);
        let o = self.coerce(o, Kind::Val);
        self.mov(t, o);
        self.exit_tries(0);
        self.emit(Instruction::ReturnValue { value: ValSlot(t.r) });
    }

    /// `yield` the value in slot `v`, then act on how the generator was
    /// resumed: `throw(e)` throws, `return(x)` returns.
    fn yield_value(&mut self, v: u16) -> Op {
        let received = self.alloc_v();
        let mode = self.alloc_s();
        self.emit(Instruction::Yield { value: ValSlot(v), received: ValSlot(received), mode: NumSlot(mode) });
        let one = self.num_const(1.0);
        let not_throw = self.emit(Instruction::JumpIfCompare { op: CmpOp::Eq, when: false, left: NumSlot(mode), right: NumSlot(one), to: 0 });
        self.emit(Instruction::Throw { value: ValSlot(received) });
        self.patch_here(not_throw);
        let two = self.num_const(2.0);
        let not_return = self.emit(Instruction::JumpIfCompare { op: CmpOp::Eq, when: false, left: NumSlot(mode), right: NumSlot(two), to: 0 });
        let m = self.mark();
        self.return_op(Op { k: Kind::Val, r: received });
        self.reset(m);
        self.patch_here(not_return);
        Op { k: Kind::Val, r: received }
    }

    /// `yield* v`: forward every resumption to the inner iterator.
    fn yield_star(&mut self, v: u16) -> Op {
        let it = self.alloc_v();
        self.emit(Instruction::GetIterator { value: ValSlot(v), output: ValSlot(it) });
        let received = self.alloc_v();
        self.emit(Instruction::LoadUndefined { output: ValSlot(received) });
        let mode = self.alloc_s();
        self.emit(Instruction::LoadNumber { output: NumSlot(mode), value: 0.0 });
        let res = self.alloc_v();
        let done = self.alloc_s();
        let top = self.here();
        self.emit(Instruction::IteratorResume {
            iterator: ValSlot(it),
            mode: NumSlot(mode),
            value: ValSlot(received),
            output: ValSlot(res),
            done: BoolSlot(done),
        });
        let exit = self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(done), to: 0 });
        self.emit(Instruction::Yield { value: ValSlot(res), received: ValSlot(received), mode: NumSlot(mode) });
        self.emit(Instruction::Jump { to: top });
        self.patch_here(exit);
        let two = self.num_const(2.0);
        let not_return = self.emit(Instruction::JumpIfCompare { op: CmpOp::Eq, when: false, left: NumSlot(mode), right: NumSlot(two), to: 0 });
        let m = self.mark();
        self.return_op(Op { k: Kind::Val, r: res });
        self.reset(m);
        self.patch_here(not_return);
        Op { k: Kind::Val, r: res }
    }

    /// Bind the loop variable from the element in slot `el` and run the body.
    fn loop_body(&mut self, elem: VarId, el: u16, body: &[HStmt]) {
        let bm = self.mark();
        let ek = self.var_info(elem).kind();
        let el = self.coerce(Op { k: Kind::Val, r: el }, ek);
        self.homes.remove(&elem);
        self.init_var(elem, Some(el));
        self.block(body);
        self.reset(bm);
    }

    /// `for (x of iterable)` through the iterator protocol (generators,
    /// user iterables, `any`). Leaving early calls the iterator's `return()`.
    fn for_of_iterator(&mut self, label: &Option<Name>, elem: VarId, iter: &HExpr, body: &[HStmt]) {
        let m = self.mark();
        let src = self.expr(iter);
        let src = self.coerce(src, Kind::Val);
        let it = self.alloc_v();
        self.emit(Instruction::GetIterator { value: ValSlot(src.r), output: ValSlot(it) });
        let exc = self.alloc_v();
        let begin = self.emit(Instruction::TryBegin { catch: 0, exception: ValSlot(exc) });
        self.tries.push(TryCtx {
            finalizer: None,
            has_handler: true,
            close_iterator: Some(it),
        });
        self.loops.push(LoopCtx {
            label: label.clone(),
            is_switch: false,
            breaks: vec![],
            continues: vec![],
            try_depth: self.tries.len(),
                    region_depth: self.regions.len(),
        });
        let el = self.alloc_v();
        let top = self.here();
        let exit = self.emit(Instruction::IteratorStep { iterator: ValSlot(it), output: ValSlot(el), to: 0 });
        self.loop_body(elem, el, body);
        self.emit(Instruction::Jump { to: top });
        let ctx = self.loops.pop().unwrap();
        for c in ctx.continues {
            self.patch(c, top);
        }
        // `break`: close the iterator; running out: it is already done.
        let has_breaks = !ctx.breaks.is_empty();
        for b in ctx.breaks {
            self.patch_here(b);
        }
        if has_breaks {
            self.emit(Instruction::IteratorClose { iterator: ValSlot(it) });
        }
        self.patch_here(exit);
        self.tries.pop();
        self.emit(Instruction::TryEnd);
        let end = self.jump_placeholder();
        // An exception in the body: close the iterator, then rethrow.
        self.patch_here(begin);
        self.emit(Instruction::IteratorClose { iterator: ValSlot(it) });
        self.emit(Instruction::Throw { value: ValSlot(exc) });
        self.patch_here(end);
        self.emit(Instruction::LoadUndefined { output: ValSlot(it) });
        self.reset(m);
    }

    /// `for await (x of iterable)`
    fn for_await(&mut self, label: &Option<Name>, elem: VarId, iter: &HExpr, body: &[HStmt]) {
        let m = self.mark();
        let src = self.expr(iter);
        let src = self.coerce(src, Kind::Val);
        let it = self.alloc_v();
        let sync = self.alloc_s();
        self.emit(Instruction::GetAsyncIterator { value: ValSlot(src.r), output: ValSlot(it), is_sync: BoolSlot(sync) });
        self.loops.push(LoopCtx {
            label: label.clone(),
            is_switch: false,
            breaks: vec![],
            continues: vec![],
            try_depth: self.tries.len(),
                    region_depth: self.regions.len(),
        });
        let res = self.alloc_v();
        let el = self.alloc_v();
        let top = self.here();
        self.emit(Instruction::IteratorNextResult { iterator: ValSlot(it), output: ValSlot(res) });
        let r = self.await_value(res);
        let exit = self.emit(Instruction::UnpackIteratorResult { result: ValSlot(r.r), output: ValSlot(el), to: 0 });
        let async_it = self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(sync), to: 0 });
        let v = self.await_value(el);
        self.emit(Instruction::CopyValue { to: ValSlot(el), from: ValSlot(v.r) });
        self.patch_here(async_it);
        self.loop_body(elem, el, body);
        self.emit(Instruction::Jump { to: top });
        let ctx = self.loops.pop().unwrap();
        for c in ctx.continues {
            self.patch(c, top);
        }
        self.patch_here(exit);
        for b in ctx.breaks {
            self.patch_here(b);
        }
        self.emit(Instruction::LoadUndefined { output: ValSlot(it) });
        self.reset(m);
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
                self.emit(Instruction::TryEnd);
            }
            if let Some(it) = self.tries[i].close_iterator {
                self.emit(Instruction::IteratorClose { iterator: ValSlot(it) });
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
        let rd = self.regions.len();
        let exc = self.alloc_v();
        let begin = self.emit(Instruction::TryBegin { catch: 0, exception: ValSlot(exc) });
        self.tries.push(TryCtx {
            finalizer: finalizer.cloned(),
            has_handler: true,
            close_iterator: None,
        });
        self.block(block);
        self.tries.pop();
        self.emit(Instruction::TryEnd);
        if let Some(f) = finalizer {
            self.block(f);
        }
        let end = self.jump_placeholder();
        self.patch_here(begin);
        // An exception may have left regions of the try block active.
        self.emit(Instruction::ExitRegionsTo { depth: rd as u16 });
        let has_handler = catch_var.is_some() || !handler.is_empty() || finalizer.is_none();
        let mut end2 = None;
        if has_handler {
            // Protect the handler too when there is a finalizer.
            let (exc2, begin2) = if finalizer.is_some() {
                let exc2 = self.alloc_v();
                let b2 = self.emit(Instruction::TryBegin { catch: 0, exception: ValSlot(exc2) });
                self.tries.push(TryCtx {
                    finalizer: finalizer.cloned(),
                    has_handler: true,
                    close_iterator: None,
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
                self.emit(Instruction::TryEnd);
                self.block(finalizer.unwrap());
                end2 = Some(self.jump_placeholder());
                self.patch_here(b2);
                self.emit(Instruction::ExitRegionsTo { depth: rd as u16 });
                self.block(finalizer.unwrap());
                self.emit(Instruction::Throw { value: ValSlot(exc2) });
            }
        } else if let Some(f) = finalizer {
            self.block(f);
            self.emit(Instruction::Throw { value: ValSlot(exc) });
        }
        self.patch_here(end);
        if let Some(e2) = end2 {
            self.patch_here(e2);
        }
    }

    fn switch(&mut self, label: &Option<Name>, disc: &HExpr, cases: &[(Option<HExpr>, Vec<HStmt>)]) {
        let m = self.mark();
        let d = self.expr(disc);
        // Keep the discriminant in its own slot.
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
                self.emit(Instruction::JumpIfCompare { op: CmpOp::Eq, when: true, left: NumSlot(dr.r), right: NumSlot(tv.r), to: 0 })
            } else {
                let a = self.coerce(dr, Kind::Val);
                let b = self.coerce(tv, Kind::Val);
                let c = self.alloc_s();
                self.emit(Instruction::StrictEquals { output: BoolSlot(c), left: ValSlot(a.r), right: ValSlot(b.r), equal: true });
                self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(c), to: 0 })
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
                    region_depth: self.regions.len(),
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
            self.emit(Instruction::Release { first: ValSlot(bm.1), count: self.v_top - bm.1 });
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
                let j = self.emit(Instruction::JumpIfCompare { op: *op, when: sense, left: NumSlot(x.r), right: NumSlot(y.r), to: 0 });
                self.reset(m);
                vec![j]
            }
            _ => {
                let m = self.mark();
                let o = self.expr(e);
                let o = self.big_truthy(o);
                let j = match o.k {
                    Kind::Big => unreachable!(),
                    Kind::Bool => {
                        if sense {
                            self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(o.r), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(o.r), to: 0 })
                        }
                    }
                    Kind::Num => {
                        let t = self.alloc_s();
                        self.emit(Instruction::NumberIsTruthy { output: BoolSlot(t), value: NumSlot(o.r) });
                        if sense {
                            self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(t), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(t), to: 0 })
                        }
                    }
                    Kind::Val => {
                        if sense {
                            self.emit(Instruction::JumpIfTruthy { when: true, value: ValSlot(o.r), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIfTruthy { when: false, value: ValSlot(o.r), to: 0 })
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
                    && let Home::Scalar(r) = self.home(*v)
                    && self.var_info(*v).kind() == Kind::Num
                {
                    self.emit(Instruction::AddConstant { output: NumSlot(r), left: NumSlot(r), constant: if *inc { 1.0 } else { -1.0 } });
                } else if let Place::Var(v) = &**place
                    && let Home::Value(r) = self.home(*v)
                {
                    self.emit(Instruction::DynamicAddConstant { output: ValSlot(r), value: ValSlot(r), constant: if *inc { 1.0 } else { -1.0 } });
                } else {
                    self.update(place, *inc, true, e.kind() == Kind::Big);
                }
            }
            HK::Call(c) => {
                let r = self.region_of(e);
                self.call_region_for(c, r);
                self.call(c, None);
            }
            HK::Assign(place, value) => {
                // Assign straight into a local's slot.
                if let Place::Var(v) = &**place
                    && let Home::Scalar(r) | Home::Value(r) = self.home(*v)
                {
                    let k = self.var_info(*v).kind();
                    self.expr_into(value, Op { k, r });
                } else {
                    let pr = self.eval_place(place);
                    let v = self.expr(value);
                    self.store_place(&pr, v);
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

    /// Evaluate into a specific slot.
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
                self.emit(Instruction::AddConstant { output: NumSlot(d.r), left: NumSlot(x.r), constant: if *op == NumOp::Add { k } else { -k } });
            } else {
                let x = self.expr(a);
                let y = self.expr(b);
                self.num_op(*op, d.r, x.r, y.r);
            }
            self.reset(m);
            return;
        }
        // Element/field reads straight into the destination.
        if let HK::Read(p) = &e.kind
            && !matches!(**p, Place::Var(_))
        {
            let m = self.mark();
            let pr = self.eval_place(p);
            let direct = match (&pr, d.k) {
                (PlaceRef::Index { a, i, k: Kind::Num }, Kind::Num) => Some(Instruction::ReadElementNumber { output: NumSlot(d.r), array: ValSlot(*a), index: NumSlot(*i) }),
                (PlaceRef::Index { a, i, k: Kind::Val }, Kind::Val) => Some(Instruction::ReadElement { output: ValSlot(d.r), array: ValSlot(*a), index: NumSlot(*i) }),
                (PlaceRef::Field { o, slot, name }, Kind::Num) => Some(self.with_field_name(name, Instruction::ReadFieldNumber { output: NumSlot(d.r), object: ValSlot(*o), field: *slot })),
                (PlaceRef::Field { o, slot, name }, Kind::Val) => Some(self.with_field_name(name, Instruction::ReadField { output: ValSlot(d.r), object: ValSlot(*o), field: *slot })),
                (PlaceRef::Prop { o, p }, Kind::Val) => Some(Instruction::ReadProperty { output: ValSlot(d.r), object: ValSlot(*o), property: *p }),
                (PlaceRef::ArrLen { a }, Kind::Num) => Some(Instruction::ArrayLength { output: NumSlot(d.r), array: ValSlot(*a) }),
                (PlaceRef::StrLen { s }, Kind::Num) => Some(Instruction::StringLength { output: NumSlot(d.r), string: ValSlot(*s) }),
                _ => None,
            };
            match direct {
                Some(ins) => {
                    self.emit(ins);
                }
                None => {
                    let o = self.load_place(&pr, d.k);
                    let o = self.coerce(o, d.k);
                    self.mov(d, o);
                }
            }
            self.reset(m);
            return;
        }
        if let HK::Binary(op, a, b) = &e.kind
            && e.kind() == d.k
            && !matches!(op, HBin::DynNum(_))
        {
            let m = self.mark();
            let x = self.operand(*op, a);
            let y = self.operand(*op, b);
            self.binop_into(*op, x, y, Some(d.r));
            self.reset(m);
            return;
        }
        let m = self.mark();
        // A literal goes straight into the destination.
        match (&e.kind, d.k) {
            (HK::Num(v), Kind::Num) if !self.num_regs.contains_key(&v.to_bits()) => {
                self.emit(Instruction::LoadNumber { value: *v, output: NumSlot(d.r) });
                return;
            }
            (HK::Bool(v), Kind::Bool) => {
                self.emit(Instruction::LoadBool { value: *v, output: BoolSlot(d.r) });
                return;
            }
            (HK::BigInt(v), Kind::Big) => {
                self.load_big(*v, d.r);
                return;
            }
            _ => {}
        }
        // Dynamic arithmetic stored as a value: produce the boxed result directly.
        if d.k == Kind::Val
            && let HK::Cast(x) = &e.kind
            && let HK::Binary(op @ HBin::DynNum(nop), a, b) = &x.kind
        {
            let xa = self.operand(*op, a);
            let xb = self.operand(*op, b);
            if xa.k == Kind::Val || xb.k == Kind::Val {
                let xa = self.coerce(xa, Kind::Val);
                let xb = self.coerce(xb, Kind::Val);
                self.emit(Instruction::DynamicArithmeticToValue { op: *nop, output: ValSlot(d.r), left: ValSlot(xa.r), right: ValSlot(xb.r) });
            } else {
                let t = self.alloc_s();
                self.num_op(*nop, t, xa.r, xb.r);
                self.emit(Instruction::BoxNumber { output: ValSlot(d.r), value: NumSlot(t) });
            }
            self.reset(m);
            return;
        }
        let o = self.expr(e);
        // Box/unbox straight into the destination instead of via a temporary.
        match (o.k, d.k) {
            (Kind::Num, Kind::Val) => self.emit(Instruction::BoxNumber { output: ValSlot(d.r), value: NumSlot(o.r) }),
            (Kind::Bool, Kind::Val) => self.emit(Instruction::BoxBool { output: ValSlot(d.r), value: BoolSlot(o.r) }),
            (Kind::Big, Kind::Val) => self.emit(Instruction::BoxBigInt { output: ValSlot(d.r), value: BigSlot(o.r) }),
            _ => {
                let o = self.coerce(o, d.k);
                self.mov(d, o);
                0
            }
        };
        self.reset(m);
    }

    fn num_op(&mut self, op: NumOp, d: u16, a: u16, b: u16) {
        self.emit(match op {
            NumOp::Add => Instruction::Arithmetic { op: NumOp::Add, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
            NumOp::Sub => Instruction::Arithmetic { op: NumOp::Sub, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
            NumOp::Mul => Instruction::Arithmetic { op: NumOp::Mul, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
            NumOp::Div => Instruction::Arithmetic { op: NumOp::Div, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
            NumOp::Mod => Instruction::Arithmetic { op: NumOp::Mod, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
            op => Instruction::Arithmetic { op, output: NumSlot(d), left: NumSlot(a), right: NumSlot(b) },
        });
    }

    /// Evaluate; the result has kind `e.kind()`. May return a variable's own slot.
    fn expr(&mut self, e: &HExpr) -> Op {
        let k = e.kind();
        let o = self.expr_raw(e);
        self.coerce(o, k)
    }

    fn expr_raw(&mut self, e: &HExpr) -> Op {
        let k = e.kind();
        match &e.kind {
            HK::Num(v) => {
                if let Some(r) = self.num_regs.get(&v.to_bits()) {
                    return Op { k: Kind::Num, r: *r };
                }
                let d = self.alloc_s();
                self.emit(Instruction::LoadNumber { output: NumSlot(d), value: *v });
                Op { k: Kind::Num, r: d }
            }
            HK::Bool(v) => {
                let d = self.alloc_s();
                self.emit(Instruction::LoadBool { output: BoolSlot(d), value: *v });
                Op { k: Kind::Bool, r: d }
            }
            HK::BigInt(v) => {
                let d = self.alloc(Kind::Big);
                self.load_big(*v, d.r);
                d
            }
            HK::Str(s) => {
                let d = self.alloc_v();
                let kk = self.str_const(s);
                self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                Op { k: Kind::Val, r: d }
            }
            HK::Null => {
                let d = self.alloc_v();
                self.emit(Instruction::LoadNull { output: ValSlot(d) });
                Op { k: Kind::Val, r: d }
            }
            HK::Undefined => {
                let d = self.alloc_v();
                self.emit(Instruction::LoadUndefined { output: ValSlot(d) });
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
                self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                Op { k: Kind::Val, r: d }
            }
            HK::StmtExpr(stmts, x) => {
                for st in stmts {
                    self.stmt(st);
                }
                self.expr(x)
            }
            HK::Await(x) => {
                let v = self.expr(x);
                let v = self.coerce(v, Kind::Val);
                self.await_value(v.r)
            }
            HK::Yield(x) => {
                let v = self.expr(x);
                let v = self.coerce(v, Kind::Val);
                let v = if self.cg.hir.funcs[self.fid as usize].is_async { self.await_value(v.r) } else { v };
                self.yield_value(v.r)
            }
            HK::YieldStar(x) => {
                let v = self.expr(x);
                let v = self.coerce(v, Kind::Val);
                self.yield_star(v.r)
            }
            HK::Delete(o, key) => {
                let o = self.expr(o);
                let o = self.coerce(o, Kind::Val);
                let kk = self.expr(key);
                let kk = self.coerce(kk, Kind::Val);
                self.emit(Instruction::DeleteDynamic { object: ValSlot(o.r), key: ValSlot(kk.r) });
                let d = self.alloc_s();
                self.emit(Instruction::LoadBool { output: BoolSlot(d), value: true });
                Op { k: Kind::Bool, r: d }
            }
            HK::ClassRef(c) => {
                let d = self.alloc_v();
                self.emit(Instruction::LoadClass { output: ValSlot(d), class: *c });
                Op { k: Kind::Val, r: d }
            }
            HK::Array(elems) => {
                // Unknown element type: start with number storage, like V8's
                // elements kinds; the first non-number write degrades it.
                let num = match &e.ty {
                    Type::Array(t) if t.kind() == Kind::Num => true,
                    Type::Array(t) if matches!(**t, Type::Any | Type::Unknown) => {
                        elems.iter().all(|x| matches!(x, HElem::Expr(x) if x.ty.kind() == Kind::Num || matches!(x.kind, HK::Cast(ref y) if y.ty.kind() == Kind::Num)))
                    }
                    _ => false,
                };
                let d = self.alloc_v();
                match self.region_of(e) {
                    Some(up) => self.emit(Instruction::NewArrayIn { output: ValSlot(d), numbers: num, up }),
                    None => self.emit(Instruction::NewArray { output: ValSlot(d), numbers: num }),
                };
                for el in elems {
                    let m = self.mark();
                    match el {
                        HElem::Expr(x) => {
                            let x = match &x.kind {
                                HK::Cast(y) if num && y.ty.kind() == Kind::Num => y,
                                _ => x,
                            };
                            let o = self.expr(x);
                            if num {
                                let o = self.coerce(o, Kind::Num);
                                self.emit(Instruction::PushNumber { array: ValSlot(d), value: NumSlot(o.r) });
                            } else {
                                let o = self.coerce(o, Kind::Val);
                                self.emit(Instruction::PushValue { array: ValSlot(d), value: ValSlot(o.r) });
                            }
                        }
                        HElem::Spread(x) => {
                            let o = self.expr(x);
                            let o = self.coerce(o, Kind::Val);
                            self.emit(Instruction::PushSpread { array: ValSlot(d), source: ValSlot(o.r) });
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
                    match self.region_of(e) {
                        Some(up) => self.emit(Instruction::NewObjectIn { output: ValSlot(d), shape, up }),
                        None => self.emit(Instruction::NewObject { output: ValSlot(d), shape }),
                    };
                    for p in props {
                        let HProp::Init(n, x) = p else { unreachable!() };
                        let slot = keys.iter().position(|k| k == n).unwrap() as u16;
                        let m = self.mark();
                        let o = self.expr(x);
                        self.note_field(&Some(n.clone()));
                        if o.k == Kind::Num {
                            self.emit(Instruction::WriteFieldNumber { object: ValSlot(d), field: slot, value: NumSlot(o.r) });
                        } else {
                            let o = self.coerce(o, Kind::Val);
                            self.emit(Instruction::WriteField { object: ValSlot(d), field: slot, value: ValSlot(o.r) });
                        }
                        self.reset(m);
                    }
                } else {
                    let shape = self.cg.shape(vec![]);
                    match self.region_of(e) {
                        Some(up) => self.emit(Instruction::NewObjectIn { output: ValSlot(d), shape, up }),
                        None => self.emit(Instruction::NewObject { output: ValSlot(d), shape }),
                    };
                    for p in props {
                        let m = self.mark();
                        match p {
                            HProp::Init(n, x) => {
                                let o = self.expr(x);
                                let o = self.coerce(o, Kind::Val);
                                let ps = self.prop_site(n);
                                self.emit(Instruction::WriteProperty { object: ValSlot(d), property: ps, value: ValSlot(o.r) });
                            }
                            HProp::Spread(x) => {
                                let o = self.expr(x);
                                let o = self.coerce(o, Kind::Val);
                                self.emit(Instruction::SpreadIntoObject { object: ValSlot(d), source: ValSlot(o.r) });
                            }
                            HProp::Computed(k, x) => {
                                let k = self.expr(k);
                                let k = self.coerce(k, Kind::Val);
                                let o = self.expr(x);
                                let o = self.coerce(o, Kind::Val);
                                self.emit(Instruction::WriteDynamic { object: ValSlot(d), key: ValSlot(k.r), value: ValSlot(o.r) });
                            }
                            HProp::Accessor { name, get, set } => {
                                let slot = |g: &mut Self, f: &Option<HExpr>| match f {
                                    Some(f) => {
                                        let o = g.expr(f);
                                        g.coerce(o, Kind::Val).r
                                    }
                                    None => {
                                        let r = g.alloc_v();
                                        g.emit(Instruction::LoadUndefined { output: ValSlot(r) });
                                        r
                                    }
                                };
                                let getter = slot(self, get);
                                let setter = slot(self, set);
                                let ps = self.prop_site(name);
                                self.emit(Instruction::DefineAccessor {
                                    object: ValSlot(d),
                                    property: ps,
                                    getter: ValSlot(getter),
                                    setter: ValSlot(setter),
                                });
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
                let av_test = if *op == LogicalOp::Nullish { av } else { self.big_truthy(av) };
                let use_a = match op {
                    LogicalOp::Nullish => {
                        if av.k == Kind::Val {
                            Some(self.emit(Instruction::JumpIfNullish { when: false, value: ValSlot(av.r), to: 0 }))
                        } else {
                            None
                        }
                    }
                    LogicalOp::And | LogicalOp::Or => {
                        // `a && b` uses a when a is falsy; `a || b` when truthy.
                        let sense = *op == LogicalOp::Or;
                        let av = av_test;
                        Some(match av.k {
                            Kind::Big => unreachable!(),
                            Kind::Bool => {
                                if sense {
                                    self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(av.r), to: 0 })
                                } else {
                                    self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(av.r), to: 0 })
                                }
                            }
                            Kind::Num => {
                                let t = self.alloc_s();
                                self.emit(Instruction::NumberIsTruthy { output: BoolSlot(t), value: NumSlot(av.r) });
                                if sense {
                                    self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(t), to: 0 })
                                } else {
                                    self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(t), to: 0 })
                                }
                            }
                            Kind::Val => {
                                if sense {
                                    self.emit(Instruction::JumpIfTruthy { when: true, value: ValSlot(av.r), to: 0 })
                                } else {
                                    self.emit(Instruction::JumpIfTruthy { when: false, value: ValSlot(av.r), to: 0 })
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
                // Keep the value in a slot the store won't clobber.
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
                    HBin::Big(_) => Kind::Big,
                    _ => Kind::Val,
                };
                let cur = self.load_place(&pr, operand_kind);
                let cur = self.coerce(cur, operand_kind);
                let v = self.expr(value);
                // `x += e` on a number local computes straight into x's slot.
                if let PlaceRef::Var(var) = pr
                    && let Home::Scalar(r) = self.home(var)
                    && operand_kind != Kind::Val
                    && self.var_info(var).kind() == operand_kind
                {
                    return self.binop_into(*op, cur, v, Some(r));
                }
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
                let t = if *op == LogicalOp::Nullish { d } else { self.big_truthy(d) };
                let skip = match (op, t.k) {
                    (_, Kind::Big) => unreachable!(),
                    (LogicalOp::Nullish, Kind::Val) => self.emit(Instruction::JumpIfNullish { when: false, value: ValSlot(d.r), to: 0 }),
                    (LogicalOp::Nullish, _) => self.jump_placeholder(),
                    (op, Kind::Val) => {
                        if *op == LogicalOp::Or {
                            self.emit(Instruction::JumpIfTruthy { when: true, value: ValSlot(d.r), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIfTruthy { when: false, value: ValSlot(d.r), to: 0 })
                        }
                    }
                    (op, Kind::Bool) => {
                        if *op == LogicalOp::Or {
                            self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(d.r), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(d.r), to: 0 })
                        }
                    }
                    (op, Kind::Num) => {
                        let t = self.alloc_s();
                        self.emit(Instruction::NumberIsTruthy { output: BoolSlot(t), value: NumSlot(d.r) });
                        if *op == LogicalOp::Or {
                            self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(t), to: 0 })
                        } else {
                            self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(t), to: 0 })
                        }
                    }
                };
                self.expr_into(value, d);
                self.store_place(&pr, d);
                self.patch_here(skip);
                d
            }
            HK::Update { place, inc, prefix } => self.update(place, *inc, *prefix, k == Kind::Big),
            HK::Call(c) => {
                let d = self.alloc(k);
                let r = self.region_of(e);
                self.call_region_for(c, r);
                self.call(c, Some(d));
                d
            }
            HK::New { class, args } => {
                let d = self.alloc_v();
                match self.region_of(e) {
                    Some(up) => self.emit(Instruction::NewInstanceIn { output: ValSlot(d), class: *class, up }),
                    None => self.emit(Instruction::NewInstance { output: ValSlot(d), class: *class }),
                };
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
                self.emit(Instruction::InstanceOf { output: BoolSlot(d), value: ValSlot(o.r), class: *c });
                Op { k: Kind::Bool, r: d }
            }
            HK::Cast(x) if k == Kind::Val && matches!(x.kind, HK::Num(_) | HK::Bool(_)) => {
                // A literal used as a value: load a pre-boxed constant.
                let v = match x.kind {
                    HK::Num(n) => Value::Num(n),
                    HK::Bool(b) => Value::Bool(b),
                    _ => unreachable!(),
                };
                let kk = self.konst(v);
                let d = self.alloc_v();
                self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                Op { k, r: d }
            }
            HK::Cast(x) if k == Kind::Val && matches!(x.kind, HK::Binary(HBin::DynNum(_), _, _)) => {
                let d = self.alloc_v();
                self.expr_into(e, Op { k, r: d });
                Op { k, r: d }
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
                    Kind::Val => self.emit(Instruction::LoadUndefined { output: ValSlot(d.r) }),
                    Kind::Num => self.emit(Instruction::LoadNumber { output: NumSlot(d.r), value: f64::NAN }),
                    Kind::Bool => self.emit(Instruction::LoadBool { output: BoolSlot(d.r), value: false }),
                    Kind::Big => {
                        self.load_big(0, d.r);
                        0
                    }
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
        let list: Vec<CaptureFrom> = free
            .iter()
            .map(|v| match self.home(*v) {
                Home::Cell(r) => CaptureFrom::Slot(ValSlot(r)),
                Home::Capture(i) => CaptureFrom::Capture(i),
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
        self.emit(Instruction::MakeClosure { output: ValSlot(d), function: f, captures: caps });
        Op { k: Kind::Val, r: d }
    }

    fn unary(&mut self, op: HUnary, x: &HExpr, k: Kind) -> Op {
        match op {
            HUnary::NegNum => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Num);
                let d = self.alloc_s();
                self.emit(Instruction::Negate { output: NumSlot(d), value: NumSlot(o.r) });
                Op { k: Kind::Num, r: d }
            }
            HUnary::BitNotNum => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Num);
                let d = self.alloc_s();
                self.emit(Instruction::BitNot { output: NumSlot(d), value: NumSlot(o.r) });
                Op { k: Kind::Num, r: d }
            }
            HUnary::NegBig | HUnary::BitNotBig => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Big);
                let d = self.alloc(Kind::Big);
                self.emit(if op == HUnary::NegBig {
                    Instruction::NegateBigInt { output: BigSlot(d.r), value: BigSlot(o.r) }
                } else {
                    Instruction::BitNotBigInt { output: BigSlot(d.r), value: BigSlot(o.r) }
                });
                d
            }
            HUnary::NegDyn | HUnary::BitNotDyn => {
                let o = self.expr(x);
                let o = self.coerce(o, Kind::Val);
                let d = self.alloc_v();
                self.emit(if op == HUnary::NegDyn {
                    Instruction::DynamicNegate { output: ValSlot(d), value: ValSlot(o.r) }
                } else {
                    Instruction::DynamicBitNot { output: ValSlot(d), value: ValSlot(o.r) }
                });
                Op { k: Kind::Val, r: d }
            }
            HUnary::NotBool | HUnary::Truthy => {
                let o = self.expr(x);
                let o = self.big_truthy(o);
                let t = match o.k {
                    Kind::Big => unreachable!(),
                    Kind::Bool => o,
                    Kind::Num => {
                        let d = self.alloc_s();
                        self.emit(Instruction::NumberIsTruthy { output: BoolSlot(d), value: NumSlot(o.r) });
                        Op { k: Kind::Bool, r: d }
                    }
                    Kind::Val => {
                        let d = self.alloc_s();
                        self.emit(Instruction::IsTruthy { output: BoolSlot(d), value: ValSlot(o.r) });
                        Op { k: Kind::Bool, r: d }
                    }
                };
                if op == HUnary::Truthy {
                    return t;
                }
                let d = self.alloc_s();
                self.emit(Instruction::Not { output: BoolSlot(d), value: BoolSlot(t.r) });
                Op { k: Kind::Bool, r: d }
            }
            HUnary::TypeOf => {
                let o = self.expr(x);
                let d = self.alloc_v();
                match o.k {
                    Kind::Num => {
                        let kk = self.str_const(&"number".into());
                        self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                    }
                    Kind::Bool => {
                        let kk = self.str_const(&"boolean".into());
                        self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                    }
                    Kind::Big => {
                        let kk = self.str_const(&"bigint".into());
                        self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
                    }
                    Kind::Val => {
                        self.emit(Instruction::TypeOf { output: ValSlot(d), value: ValSlot(o.r) });
                    }
                }
                Op { k: Kind::Val, r: d }
            }
            HUnary::Void => {
                self.expr_discard(x);
                let d = self.alloc_v();
                self.emit(Instruction::LoadUndefined { output: ValSlot(d) });
                Op { k: Kind::Val, r: d }
            }
            HUnary::ToNum => {
                let o = self.expr(x);
                match o.k {
                    Kind::Num => o,
                    _ => {
                        let o = self.coerce(o, Kind::Val);
                        let d = self.alloc_s();
                        self.emit(Instruction::ToNumber { output: NumSlot(d), value: ValSlot(o.r) });
                        Op { k: Kind::Num, r: d }
                    }
                }
            }
            HUnary::ToStr => {
                let o = self.expr(x);
                let d = self.alloc_v();
                match o.k {
                    Kind::Num => {
                        self.emit(Instruction::NumberToString { output: ValSlot(d), value: NumSlot(o.r) });
                    }
                    _ => {
                        let o = self.coerce(o, Kind::Val);
                        if matches!(x.ty, Type::String | Type::StrLit(_)) {
                            return o;
                        }
                        self.emit(Instruction::ToString { output: ValSlot(d), value: ValSlot(o.r) });
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
            self.emit(Instruction::AddConstant { output: NumSlot(d), left: NumSlot(x.r), constant: if nop == NumOp::Add { kv } else { -kv } });
            return Op { k: Kind::Num, r: d };
        }
        let x = self.operand(op, a);
        let y = self.operand(op, b);
        self.binop_regs(op, x, y)
    }

    /// Evaluate a binary operand. Literals feeding a dynamic operator are
    /// loaded as pre-boxed constants instead of loaded and then boxed.
    fn operand(&mut self, op: HBin, e: &HExpr) -> Op {
        let dynamic = matches!(op, HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_) | HBin::DynCmp(_) | HBin::StrictEq(_) | HBin::LooseEq(_));
        if dynamic && let HK::Num(n) = e.kind {
            let kk = self.konst(Value::Num(n));
            let d = self.alloc_v();
            self.emit(Instruction::LoadConstant { output: ValSlot(d), constant: kk });
            return Op { k: Kind::Val, r: d };
        }
        if dynamic && let HK::Binary(HBin::DynNum(nop), a, b) = &e.kind {
            let xa = self.operand(HBin::DynNum(*nop), a);
            let xb = self.operand(HBin::DynNum(*nop), b);
            if xa.k == Kind::Val || xb.k == Kind::Val {
                let xa = self.coerce(xa, Kind::Val);
                let xb = self.coerce(xb, Kind::Val);
                let d = self.alloc_v();
                self.emit(Instruction::DynamicArithmeticToValue { op: *nop, output: ValSlot(d), left: ValSlot(xa.r), right: ValSlot(xb.r) });
                return Op { k: Kind::Val, r: d };
            }
            let d = self.alloc_s();
            self.num_op(*nop, d, xa.r, xb.r);
            return Op { k: Kind::Num, r: d };
        }
        self.expr(e)
    }

    fn binop_regs(&mut self, op: HBin, x: Op, y: Op) -> Op {
        self.binop_into(op, x, y, None)
    }

    /// Binary op; writes into `dst` when given (its kind must match the result kind).
    fn binop_into(&mut self, op: HBin, x: Op, y: Op, dst: Option<u16>) -> Op {
        match op {
            HBin::Big(nop) => {
                let x = self.coerce(x, Kind::Big);
                let y = self.coerce(y, Kind::Big);
                let d = dst.unwrap_or_else(|| self.alloc(Kind::Big).r);
                self.emit(Instruction::BigIntArithmetic { op: nop, output: BigSlot(d), left: BigSlot(x.r), right: BigSlot(y.r) });
                Op { k: Kind::Big, r: d }
            }
            HBin::BigCmp(cop) => {
                let x = self.coerce(x, Kind::Big);
                let y = self.coerce(y, Kind::Big);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(Instruction::CompareBigInts { op: cop, output: BoolSlot(d), left: BigSlot(x.r), right: BigSlot(y.r) });
                Op { k: Kind::Bool, r: d }
            }
            HBin::Num(nop) => {
                let x = self.coerce(x, Kind::Num);
                let y = self.coerce(y, Kind::Num);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.num_op(nop, d, x.r, y.r);
                Op { k: Kind::Num, r: d }
            }
            HBin::NumCmp(c) => {
                let x = self.coerce(x, Kind::Num);
                let y = self.coerce(y, Kind::Num);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(match c {
                    CmpOp::Lt => Instruction::Compare { op: CmpOp::Lt, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                    CmpOp::Le => Instruction::Compare { op: CmpOp::Le, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                    CmpOp::Gt => Instruction::Compare { op: CmpOp::Gt, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                    CmpOp::Ge => Instruction::Compare { op: CmpOp::Ge, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                    CmpOp::Eq => Instruction::Compare { op: CmpOp::Eq, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                    CmpOp::Ne => Instruction::Compare { op: CmpOp::Ne, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) },
                });
                Op { k: Kind::Bool, r: d }
            }
            HBin::BoolEq(eq) => {
                let x = self.coerce(x, Kind::Bool);
                let y = self.coerce(y, Kind::Bool);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(if eq {
                    Instruction::CompareBools { equal: true, output: BoolSlot(d), left: BoolSlot(x.r), right: BoolSlot(y.r) }
                } else {
                    Instruction::CompareBools { equal: false, output: BoolSlot(d), left: BoolSlot(x.r), right: BoolSlot(y.r) }
                });
                Op { k: Kind::Bool, r: d }
            }
            HBin::StrictEq(eq) => {
                let d = dst.unwrap_or_else(|| self.alloc_s());
                if x.k == Kind::Num && y.k == Kind::Num {
                    self.emit(if eq {
                        Instruction::Compare { op: CmpOp::Eq, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) }
                    } else {
                        Instruction::Compare { op: CmpOp::Ne, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) }
                    });
                } else if x.k == Kind::Bool && y.k == Kind::Bool {
                    self.emit(if eq {
                        Instruction::CompareBools { equal: true, output: BoolSlot(d), left: BoolSlot(x.r), right: BoolSlot(y.r) }
                    } else {
                        Instruction::CompareBools { equal: false, output: BoolSlot(d), left: BoolSlot(x.r), right: BoolSlot(y.r) }
                    });
                } else {
                    let x = self.coerce(x, Kind::Val);
                    let y = self.coerce(y, Kind::Val);
                    self.emit(Instruction::StrictEquals { output: BoolSlot(d), left: ValSlot(x.r), right: ValSlot(y.r), equal: eq });
                }
                Op { k: Kind::Bool, r: d }
            }
            HBin::LooseEq(eq) => {
                let d = dst.unwrap_or_else(|| self.alloc_s());
                if x.k == Kind::Num && y.k == Kind::Num {
                    self.emit(if eq {
                        Instruction::Compare { op: CmpOp::Eq, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) }
                    } else {
                        Instruction::Compare { op: CmpOp::Ne, output: BoolSlot(d), left: NumSlot(x.r), right: NumSlot(y.r) }
                    });
                } else {
                    let x = self.coerce(x, Kind::Val);
                    let y = self.coerce(y, Kind::Val);
                    self.emit(Instruction::LooseEquals { output: BoolSlot(d), left: ValSlot(x.r), right: ValSlot(y.r), equal: eq });
                }
                Op { k: Kind::Bool, r: d }
            }
            HBin::Concat => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_v());
                self.emit(Instruction::Concat { output: ValSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Val, r: d }
            }
            HBin::StrCmp(c) => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(Instruction::CompareStrings { op: c, output: BoolSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Bool, r: d }
            }
            HBin::DynAdd => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_v());
                self.emit(Instruction::DynamicAdd { output: ValSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Val, r: d }
            }
            HBin::DynNum(nop) => {
                if x.k == Kind::Num && y.k == Kind::Num {
                    let d = dst.unwrap_or_else(|| self.alloc_s());
                    self.num_op(nop, d, x.r, y.r);
                    return Op { k: Kind::Num, r: d };
                }
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(Instruction::DynamicArithmetic { op: nop, output: NumSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Num, r: d }
            }
            HBin::DynNumeric(nop) => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_v());
                self.emit(Instruction::DynamicArithmeticToValue { op: nop, output: ValSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Val, r: d }
            }
            HBin::DynCmp(c) => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(Instruction::DynamicCompare { op: c, output: BoolSlot(d), left: ValSlot(x.r), right: ValSlot(y.r) });
                Op { k: Kind::Bool, r: d }
            }
            HBin::In => {
                let x = self.coerce(x, Kind::Val);
                let y = self.coerce(y, Kind::Val);
                let d = dst.unwrap_or_else(|| self.alloc_s());
                self.emit(Instruction::In { output: BoolSlot(d), key: ValSlot(x.r), object: ValSlot(y.r) });
                Op { k: Kind::Bool, r: d }
            }
        }
    }

    /// `++`/`--` on a place; `big` when it holds a `bigint`.
    fn update(&mut self, place: &Place, inc: bool, prefix: bool, big: bool) -> Op {
        let pr = self.eval_place(place);
        if big {
            let cur = self.load_place(&pr, Kind::Big);
            let cur = self.coerce(cur, Kind::Big);
            let old = if prefix {
                cur
            } else {
                let t = self.alloc(Kind::Big);
                self.mov(t, cur);
                t
            };
            let one = self.alloc(Kind::Big);
            self.load_big(1, one.r);
            let new = self.alloc(Kind::Big);
            let op = if inc { NumOp::Add } else { NumOp::Sub };
            self.emit(Instruction::BigIntArithmetic { op, output: BigSlot(new.r), left: BigSlot(old.r), right: BigSlot(one.r) });
            self.store_place(&pr, new);
            return if prefix { new } else { old };
        }
        let cur = self.load_place(&pr, Kind::Num);
        let cur = match cur.k {
            Kind::Big => unreachable!(),
            Kind::Num => cur,
            Kind::Val => {
                let d = self.alloc_s();
                self.emit(Instruction::ToNumber { output: NumSlot(d), value: ValSlot(cur.r) });
                Op { k: Kind::Num, r: d }
            }
            Kind::Bool => self.coerce(cur, Kind::Num),
        };
        // Keep the old value (postfix) out of the variable's own slot.
        let old = if !prefix {
            let t = self.alloc_s();
            self.emit(Instruction::CopyNumber { to: NumSlot(t), from: NumSlot(cur.r) });
            Op { k: Kind::Num, r: t }
        } else {
            cur
        };
        let new = self.alloc_s();
        self.emit(Instruction::AddConstant { output: NumSlot(new), left: NumSlot(old.r), constant: if inc { 1.0 } else { -1.0 } });
        let new = Op { k: Kind::Num, r: new };
        self.store_place(&pr, new);
        if prefix { new } else { old }
    }

    // ───────────── places ─────────────

    fn opt_check(&mut self, o: Op, opt: bool) {
        if opt {
            let j = self.emit(Instruction::JumpIfNullish { when: true, value: ValSlot(o.r), to: 0 });
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
                let name = match &obj.ty {
                    Type::Class(c, _) => self.cg.hir.classes[*c as usize].fields.get(*slot as usize).cloned(),
                    _ => None,
                };
                PlaceRef::Field { o: o.r, slot: *slot as u16, name }
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
            Place::Accessor { obj, get, set, opt } => {
                let o = self.expr(obj);
                let o = self.coerce(o, Kind::Val);
                self.opt_check(o, *opt);
                let class = match &obj.ty {
                    Type::Class(c, _) => *c,
                    _ => u32::MAX,
                };
                PlaceRef::Accessor { o: o.r, class, get: *get, set: *set }
            }
        }
    }

    /// Load a place's value; `want` hints the preferred kind (result may differ).
    fn load_place(&mut self, p: &PlaceRef, want: Kind) -> Op {
        match *p {
            PlaceRef::Var(v) => {
                let k = self.var_info(v).kind();
                match self.home(v) {
                    Home::Scalar(r) => Op { k, r },
                    Home::Value(r) => Op { k: Kind::Val, r },
                    Home::Cell(c) => {
                        let d = self.alloc_v();
                        self.emit(Instruction::ReadCell { output: ValSlot(d), cell: ValSlot(c) });
                        Op { k: Kind::Val, r: d }
                    }
                    Home::Capture(i) => {
                        let d = self.alloc_v();
                        self.emit(Instruction::ReadCapture { output: ValSlot(d), capture: i });
                        Op { k: Kind::Val, r: d }
                    }
                    Home::GlobalScalar(g) if k == Kind::Big => {
                        let d = self.alloc(k);
                        self.emit(Instruction::ReadGlobalBigInt { output: BigSlot(d.r), global: g });
                        d
                    }
                    Home::GlobalScalar(g) => {
                        let d = self.alloc_s();
                        if k == Kind::Bool {
                            self.emit(Instruction::ReadGlobalBool { output: BoolSlot(d), global: g });
                        } else {
                            self.emit(Instruction::ReadGlobalNumber { output: NumSlot(d), global: g });
                        }
                        Op { k, r: d }
                    }
                    Home::GlobalValue(g) => {
                        let d = self.alloc_v();
                        self.emit(Instruction::ReadGlobal { output: ValSlot(d), global: g });
                        Op { k: Kind::Val, r: d }
                    }
                }
            }
            PlaceRef::Field { o, slot, ref name } => {
                self.note_field(name);
                if want == Kind::Num {
                    let d = self.alloc_s();
                    self.emit(Instruction::ReadFieldNumber { output: NumSlot(d), object: ValSlot(o), field: slot });
                    Op { k: Kind::Num, r: d }
                } else {
                    let d = self.alloc_v();
                    self.emit(Instruction::ReadField { output: ValSlot(d), object: ValSlot(o), field: slot });
                    Op { k: Kind::Val, r: d }
                }
            }
            PlaceRef::Prop { o, p } => {
                let d = self.alloc_v();
                self.emit(Instruction::ReadProperty { output: ValSlot(d), object: ValSlot(o), property: p });
                Op { k: Kind::Val, r: d }
            }
            PlaceRef::Index { a, i, k } => {
                if k == Kind::Num {
                    let d = self.alloc_s();
                    self.emit(Instruction::ReadElementNumber { output: NumSlot(d), array: ValSlot(a), index: NumSlot(i) });
                    Op { k: Kind::Num, r: d }
                } else {
                    let d = self.alloc_v();
                    self.emit(Instruction::ReadElement { output: ValSlot(d), array: ValSlot(a), index: NumSlot(i) });
                    Op { k: Kind::Val, r: d }
                }
            }
            PlaceRef::Dyn { o, k } => {
                let d = self.alloc_v();
                self.emit(Instruction::ReadDynamic { output: ValSlot(d), object: ValSlot(o), key: ValSlot(k) });
                Op { k: Kind::Val, r: d }
            }
            PlaceRef::ArrLen { a } => {
                let d = self.alloc_s();
                self.emit(Instruction::ArrayLength { output: NumSlot(d), array: ValSlot(a) });
                Op { k: Kind::Num, r: d }
            }
            PlaceRef::StrLen { s } => {
                let d = self.alloc_s();
                self.emit(Instruction::StringLength { output: NumSlot(d), string: ValSlot(s) });
                Op { k: Kind::Num, r: d }
            }
            PlaceRef::Accessor { o, class, get, .. } => {
                let d = self.alloc(if want == Kind::Num { Kind::Num } else { Kind::Val });
                match get {
                    Some(slot) => self.method_call_raw(class, slot, o, vec![], Some(d)),
                    None => {
                        self.emit(Instruction::LoadUndefined { output: ValSlot(d.r) });
                    }
                }
                d
            }
        }
    }

    /// Virtual call `o.<vtable slot>(args)` with arguments already in slots.
    fn method_call_raw(&mut self, class: u32, slot: u32, o: u16, args: Vec<(Kind, u16)>, dst: Option<Op>) {
        let n = args.len();
        self.calls.push(CallSite {
            target: CallTarget::Method { class, slot },
            this: Some(ValSlot(o)),
            args,
            spread: vec![false; n],
            has_spread: false,
            output: dst.map(|d| (d.k, d.r)),
            packed_rest: false,
            region: None,
        });
        let ci = self.calls.len() as u32 - 1;
        self.emit(Instruction::Call { site: ci });
    }

    fn store_var(&mut self, v: VarId, val: Op) {
        let k = self.var_info(v).kind();
        match self.home(v) {
            Home::Scalar(r) => {
                let val = self.coerce(val, k);
                self.mov(Op { k, r }, val);
            }
            Home::Value(r) => {
                let val = self.coerce(val, Kind::Val);
                self.mov(Op { k: Kind::Val, r }, val);
            }
            Home::Cell(c) => {
                let val = self.coerce(val, k);
                let val = self.coerce(val, Kind::Val);
                self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(val.r) });
            }
            Home::Capture(i) => {
                let val = self.coerce(val, k);
                let val = self.coerce(val, Kind::Val);
                self.emit(Instruction::WriteCapture { capture: i, value: ValSlot(val.r) });
            }
            Home::GlobalScalar(g) => {
                let val = self.coerce(val, k);
                if k == Kind::Big {
                    self.emit(Instruction::WriteGlobalBigInt { global: g, value: BigSlot(val.r) });
                } else if k == Kind::Bool {
                    self.emit(Instruction::WriteGlobalBool { global: g, value: BoolSlot(val.r) });
                } else {
                    self.emit(Instruction::WriteGlobalNumber { global: g, value: NumSlot(val.r) });
                }
            }
            Home::GlobalValue(g) => {
                let val = self.coerce(val, Kind::Val);
                self.emit(Instruction::WriteGlobal { global: g, value: ValSlot(val.r) });
            }
        }
    }

    fn store_place(&mut self, p: &PlaceRef, val: Op) {
        match *p {
            PlaceRef::Var(v) => self.store_var(v, val),
            PlaceRef::Field { o, slot, ref name } => {
                self.note_field(name);
                if val.k == Kind::Num {
                    self.emit(Instruction::WriteFieldNumber { object: ValSlot(o), field: slot, value: NumSlot(val.r) });
                } else {
                    let val = self.coerce(val, Kind::Val);
                    self.emit(Instruction::WriteField { object: ValSlot(o), field: slot, value: ValSlot(val.r) });
                }
            }
            PlaceRef::Prop { o, p } => {
                let val = self.coerce(val, Kind::Val);
                self.emit(Instruction::WriteProperty { object: ValSlot(o), property: p, value: ValSlot(val.r) });
            }
            PlaceRef::Index { a, i, k } => {
                if k == Kind::Num {
                    let val = self.coerce(val, Kind::Num);
                    self.emit(Instruction::WriteElementNumber { array: ValSlot(a), index: NumSlot(i), value: NumSlot(val.r) });
                } else {
                    let val = self.coerce(val, Kind::Val);
                    self.emit(Instruction::WriteElement { array: ValSlot(a), index: NumSlot(i), value: ValSlot(val.r) });
                }
            }
            PlaceRef::Dyn { o, k } => {
                let val = self.coerce(val, Kind::Val);
                self.emit(Instruction::WriteDynamic { object: ValSlot(o), key: ValSlot(k), value: ValSlot(val.r) });
            }
            PlaceRef::ArrLen { a } => {
                let val = self.coerce(val, Kind::Num);
                self.emit(Instruction::SetArrayLength { array: ValSlot(a), length: NumSlot(val.r) });
            }
            PlaceRef::StrLen { .. } => {}
            PlaceRef::Accessor { o, class, set, .. } => {
                if let Some(slot) = set {
                    self.method_call_raw(class, slot, o, vec![(val.k, val.r)], None);
                }
            }
        }
    }

    // ───────────── calls ─────────────

    /// Note the result region for a call about to be emitted. It is taken
    /// when the call's `CallSite` is built, after its arguments (which may
    /// contain calls of their own) are emitted.
    fn call_region_for(&mut self, c: &HCall, r: Option<u16>) {
        self.pending_regions.push((c as *const HCall as usize, r));
    }

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
                self.emit(Instruction::MathUnary { function: n, output: NumSlot(res), value: NumSlot(a.r) });
            } else {
                let b = self.expr(plain[1]);
                self.emit(Instruction::MathBinary { function: n, output: NumSlot(res), left: NumSlot(a.r), right: NumSlot(b.r) });
            }
            if let Some(d) = dst {
                let r = self.coerce(Op { k: Kind::Num, r: res }, d.k);
                self.mov(d, r);
            }
            return true;
        }
        if let Some(t) = this
            && matches!(n, ArrMap | ArrFilter | ArrForEach | ArrSome | ArrEvery | ArrFind | ArrFindIndex | ArrReduce)
            && matches!(c.this.as_ref().unwrap().ty.non_null(), Type::Array(_))
            && plain.len() == if n == ArrReduce { 2 } else { 1 }
        {
            self.array_hof(n, t, c, plain, dst);
            return true;
        }
        match (n, this) {
            (ArrPush, Some(t)) if plain.len() == 1 && dst.is_none() => {
                let num_arr = matches!(&c.this.as_ref().unwrap().ty, Type::Array(e) if e.kind() == Kind::Num);
                let v = self.expr(plain[0]);
                if num_arr && v.k == Kind::Num {
                    self.emit(Instruction::PushNumber { array: ValSlot(t.r), value: NumSlot(v.r) });
                } else {
                    let v = self.coerce(v, Kind::Val);
                    self.emit(Instruction::PushValue { array: ValSlot(t.r), value: ValSlot(v.r) });
                }
                true
            }
            (StrCharCodeAt, Some(t)) if plain.len() == 1 && plain[0].kind() == Kind::Num => {
                let i = self.expr(plain[0]);
                let res = self.alloc_s();
                self.emit(Instruction::CharCodeAt { output: NumSlot(res), string: ValSlot(t.r), index: NumSlot(i.r) });
                if let Some(d) = dst {
                    let r = self.coerce(Op { k: Kind::Num, r: res }, d.k);
                    self.mov(d, r);
                }
                true
            }
            _ => false,
        }
    }

    /// `arr.map(f)` and friends as a bytecode loop: the callback is called with
    /// typed arguments (no boxing for `number[]`), and no native boundary is crossed.
    fn array_hof(&mut self, n: Native, arr: Op, c: &HCall, args: Vec<&HExpr>, dst: Option<Op>) {
        use Native::*;
        let recv_ty = c.this.as_ref().unwrap().ty.non_null();
        let Type::Array(elem_ty) = &recv_ty else { unreachable!() };
        let ek = if elem_ty.kind() == Kind::Num { Kind::Num } else { Kind::Val };
        let elem_kind = elem_ty.kind();
        let cb = args[0];
        let (nparams, cb_param_kinds, cb_ret) = match &cb.ty {
            Type::Func(ft) => (
                ft.params.len().min(if n == ArrReduce { 4 } else { 3 }),
                ft.params.iter().map(crate::check::param_arrival).collect::<Vec<_>>(),
                ft.ret.kind(),
            ),
            _ => (if n == ArrReduce { 4 } else { 3 }, vec![], Kind::Val),
        };
        let inline_fid = match &cb.kind {
            HK::Closure(fid) if self.can_inline(*fid) => Some(*fid),
            _ => None,
        };
        let f = match inline_fid {
            Some(_) => Op { k: Kind::Val, r: 0 },
            None => {
                let f = self.expr(cb);
                self.coerce(f, Kind::Val)
            }
        };
        // Result accumulator.
        let res_kind = dst.map_or(Kind::Val, |d| d.k);
        let res = match n {
            ArrMap | ArrFilter => {
                let num = matches!(&c_ret_elem(&c.args, n, elem_ty), Some(k) if *k == Kind::Num);
                let d = self.alloc_v();
                let num = if n == ArrMap {
                    // Element kind of the mapped array from the call's static type.
                    num
                } else {
                    ek == Kind::Num
                };
                self.emit(Instruction::NewArray { output: ValSlot(d), numbers: num });
                Some(Op { k: Kind::Val, r: d })
            }
            ArrReduce => {
                let acc = self.alloc(res_kind);
                self.expr_into(args[1], acc);
                Some(acc)
            }
            _ => None,
        };
        let map_num = matches!(n, ArrMap) && {
            if let Some(d) = dst {
                let _ = d;
            }
            false
        };
        let _ = map_num;
        let len = self.alloc_s();
        self.emit(Instruction::ArrayLength { output: NumSlot(len), array: ValSlot(arr.r) });
        let i = self.alloc_s();
        self.emit(Instruction::LoadNumber { output: NumSlot(i), value: 0.0 });
        let found = match n {
            ArrSome | ArrEvery | ArrFind | ArrFindIndex => {
                let d = self.alloc(res_kind);
                Some(d)
            }
            _ => None,
        };
        let to_test = self.jump_placeholder();
        let top = self.here();
        let m = self.mark();
        let x = self.alloc(ek);
        if ek == Kind::Num {
            self.emit(Instruction::ReadElementNumber { output: NumSlot(x.r), array: ValSlot(arr.r), index: NumSlot(i) });
        } else {
            self.emit(Instruction::ReadElement { output: ValSlot(x.r), array: ValSlot(arr.r), index: NumSlot(i) });
        }
        // Arguments in the callback's parameter kinds (exact for the common case).
        let mut cargs: Vec<(Kind, u16)> = Vec::new();
        let mut push_arg = |g: &mut Self, idx: usize, o: Op| {
            let k = cb_param_kinds.get(idx).copied().unwrap_or(o.k);
            let o = g.coerce(o, k);
            cargs.push((o.k, o.r));
        };
        let base = if n == ArrReduce {
            push_arg(self, 0, res.unwrap());
            1
        } else {
            0
        };
        let xe = self.coerce(x, if elem_kind == Kind::Bool { Kind::Val } else { x.k });
        if nparams > base {
            push_arg(self, base, xe);
        }
        if nparams > base + 1 {
            push_arg(self, base + 1, Op { k: Kind::Num, r: i });
        }
        if nparams > base + 2 {
            push_arg(self, base + 2, arr);
        }
        let out = match n {
            ArrForEach => None,
            ArrReduce => Some(res.unwrap()),
            ArrMap => Some(self.alloc(if cb_ret == Kind::Num { Kind::Num } else { Kind::Val })),
            _ => Some(self.alloc(if cb_ret == Kind::Bool { Kind::Bool } else { Kind::Val })),
        };
        if let Some(fid) = inline_fid {
            // The element temp (and reduce's accumulator) can be bound directly.
            let args: Vec<(Op, bool)> = cargs
                .iter()
                .enumerate()
                .map(|(j, &(k, r))| {
                    let owned = (j == base && r == xe.r && k == xe.k) || (n == ArrReduce && j == 0 && Some(r) == res.map(|o| o.r));
                    (Op { k, r }, owned)
                })
                .collect();
            self.inline_call(fid, &args, out);
        } else {
            let spread = vec![false; cargs.len()];
            self.calls.push(CallSite {
                target: CallTarget::Value(ValSlot(f.r)),
                this: None,
                args: cargs,
                spread,
                has_spread: false,
                output: out.map(|o| (o.k, o.r)),
                packed_rest: false,
                region: None,
            });
            let ci = self.calls.len() as u32 - 1;
            self.emit(Instruction::Call { site: ci });
        }
        let mut early = None;
        match n {
            ArrMap => {
                let o = out.unwrap();
                let r = res.unwrap();
                if o.k == Kind::Num {
                    self.emit(Instruction::PushNumber { array: ValSlot(r.r), value: NumSlot(o.r) });
                } else {
                    self.emit(Instruction::PushValue { array: ValSlot(r.r), value: ValSlot(o.r) });
                }
            }
            ArrFilter => {
                let o = out.unwrap();
                let t = self.truthy(o);
                let skip = self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(t.r), to: 0 });
                let r = res.unwrap();
                if x.k == Kind::Num {
                    self.emit(Instruction::PushNumber { array: ValSlot(r.r), value: NumSlot(x.r) });
                } else {
                    self.emit(Instruction::PushValue { array: ValSlot(r.r), value: ValSlot(x.r) });
                }
                self.patch_here(skip);
            }
            ArrSome | ArrEvery | ArrFind | ArrFindIndex => {
                let o = out.unwrap();
                let t = self.truthy(o);
                let j = if n == ArrEvery {
                    self.emit(Instruction::JumpIf { when: false, condition: BoolSlot(t.r), to: 0 })
                } else {
                    self.emit(Instruction::JumpIf { when: true, condition: BoolSlot(t.r), to: 0 })
                };
                early = Some((j, x));
            }
            _ => {}
        }
        self.reset(m);
        self.emit(Instruction::AddConstant { output: NumSlot(i), left: NumSlot(i), constant: 1.0 });
        self.patch_here(to_test);
        self.emit(Instruction::JumpIfCompare { op: CmpOp::Lt, when: true, left: NumSlot(i), right: NumSlot(len), to: top });
        // Loop finished without an early exit.
        let fd = found;
        match n {
            ArrSome => self.load_bool(fd.unwrap(), false),
            ArrEvery => self.load_bool(fd.unwrap(), true),
            ArrFind => {
                let d = fd.unwrap();
                self.load_default(d);
            }
            ArrFindIndex => {
                let d = fd.unwrap();
                let o = self.alloc_s();
                self.emit(Instruction::LoadNumber { output: NumSlot(o), value: -1.0 });
                let o = self.coerce(Op { k: Kind::Num, r: o }, d.k);
                self.mov(d, o);
            }
            _ => {}
        }
        if let Some((j, x)) = early {
            let end = self.jump_placeholder();
            self.patch_here(j);
            let d = fd.unwrap();
            match n {
                ArrSome => self.load_bool(d, true),
                ArrEvery => self.load_bool(d, false),
                ArrFind => {
                    // x's slot was released; re-read the element.
                    let xr = self.alloc(ek);
                    if ek == Kind::Num {
                        self.emit(Instruction::ReadElementNumber { output: NumSlot(xr.r), array: ValSlot(arr.r), index: NumSlot(i) });
                    } else {
                        self.emit(Instruction::ReadElement { output: ValSlot(xr.r), array: ValSlot(arr.r), index: NumSlot(i) });
                    }
                    let o = self.coerce(xr, d.k);
                    self.mov(d, o);
                }
                ArrFindIndex => {
                    let o = self.coerce(Op { k: Kind::Num, r: i }, d.k);
                    self.mov(d, o);
                }
                _ => {}
            }
            let _ = x;
            self.patch_here(end);
        }
        if let Some(d) = dst {
            let r = match n {
                ArrMap | ArrFilter | ArrReduce => res.unwrap(),
                ArrSome | ArrEvery | ArrFind | ArrFindIndex => fd.unwrap(),
                _ => {
                    let u = self.alloc_v();
                    self.emit(Instruction::LoadUndefined { output: ValSlot(u) });
                    Op { k: Kind::Val, r: u }
                }
            };
            let r = self.coerce(r, d.k);
            self.mov(d, r);
        }
    }

    /// Whether a closure literal can be compiled inline at its call site.
    fn can_inline(&self, fid: FuncId) -> bool {
        let hf = &self.cg.hir.funcs[fid as usize];
        hf.this_var.is_none()
            && !hf.is_async
            && !hf.is_generator
            && hf.params.iter().all(|p| p.default.is_none() && !p.rest)
            && self.inline.len() < 4
            && hf.free.iter().all(|v| self.homes.contains_key(v) || self.cg.globals.contains_key(v))
    }

    fn load_default(&mut self, o: Op) {
        match o.k {
            Kind::Val => self.emit(Instruction::LoadUndefined { output: ValSlot(o.r) }),
            Kind::Num => self.emit(Instruction::LoadNumber { output: NumSlot(o.r), value: f64::NAN }),
            Kind::Bool => self.emit(Instruction::LoadBool { output: BoolSlot(o.r), value: false }),
            Kind::Big => {
                self.load_big(0, o.r);
                0
            }
        };
    }

    /// Compile function `fid`'s body in place, with `args` bound to its parameters.
    fn inline_call(&mut self, fid: FuncId, args: &[(Op, bool)], out: Option<Op>) {
        let hf = &self.cg.hir.funcs[fid as usize];
        let m = self.mark();
        for (i, p) in hf.params.iter().enumerate() {
            let info = self.var_info(p.var).clone();
            let owned = args.get(i).is_some_and(|a| a.1);
            let arg = args.get(i).map(|a| a.0);
            // An argument in a temporary we own can become the parameter's home.
            if !info.captured
                && owned
                && let Some(a) = arg
                && a.k == info.kind()
            {
                self.set_home(p.var, if a.k == Kind::Val { Home::Value(a.r) } else { Home::Scalar(a.r) });
                continue;
            }
            if info.captured {
                let c = self.alloc_v();
                self.emit(Instruction::NewCell { output: ValSlot(c) });
                if let Some(a) = arg {
                    let a = self.coerce(a, info.kind());
                    let a = self.coerce(a, Kind::Val);
                    self.emit(Instruction::WriteCell { cell: ValSlot(c), value: ValSlot(a.r) });
                }
                self.set_home(p.var, Home::Cell(c));
            } else {
                let h = self.alloc(info.kind());
                match arg {
                    Some(a) => {
                        let a = self.coerce(a, h.k);
                        self.mov(h, a);
                    }
                    None => self.load_default(h),
                }
                self.set_home(p.var, if h.k == Kind::Val { Home::Value(h.r) } else { Home::Scalar(h.r) });
            }
        }
        // A body that is a single `return e` needs no default for falling off the end.
        let single_return = matches!(&hf.body[..], [HStmt::Block(b, _)] if matches!(&b[..], [HStmt::Return(Some(_))]));
        if let Some(o) = out
            && !single_return
        {
            self.load_default(o);
        }
        self.inline.push(InlineCtx {
            out,
            exits: vec![],
            try_depth: self.tries.len(),
            loop_depth: self.loops.len(),
            region_depth: self.regions.len(),
        });
        let body = hf.body.clone();
        self.stmts(&body);
        let ctx = self.inline.pop().unwrap();
        let _ = ctx.loop_depth;
        for j in ctx.exits {
            self.patch_here(j);
        }
        self.reset(m);
    }

    /// A BigInt's truthiness as a boolean; other kinds unchanged.
    fn big_truthy(&mut self, o: Op) -> Op {
        if o.k == Kind::Big { self.truthy(o) } else { o }
    }

    fn truthy(&mut self, o: Op) -> Op {
        match o.k {
            Kind::Bool => o,
            Kind::Big => {
                let d = self.alloc_s();
                self.emit(Instruction::BigIntIsTruthy { output: BoolSlot(d), value: BigSlot(o.r) });
                Op { k: Kind::Bool, r: d }
            }
            Kind::Num => {
                let d = self.alloc_s();
                self.emit(Instruction::NumberIsTruthy { output: BoolSlot(d), value: NumSlot(o.r) });
                Op { k: Kind::Bool, r: d }
            }
            Kind::Val => {
                let d = self.alloc_s();
                self.emit(Instruction::IsTruthy { output: BoolSlot(d), value: ValSlot(o.r) });
                Op { k: Kind::Bool, r: d }
            }
        }
    }

    fn load_bool(&mut self, d: Op, v: bool) {
        let t = self.alloc_s();
        self.emit(Instruction::LoadBool { output: BoolSlot(t), value: v });
        let o = self.coerce(Op { k: Kind::Bool, r: t }, d.k);
        self.mov(d, o);
    }

    fn call_with_this(&mut self, c: &HCall, this: Option<u16>, dst: Option<Op>) {
        let (target, param_kinds, rest, pack) = match &c.callee {
            Callee::Direct { func, closure } => {
                let hf = &self.cg.hir.funcs[*func as usize];
                let kinds: Vec<Kind> = hf.params.iter().map(|p| p.arrival).collect();
                let rest = hf.params.last().is_some_and(|p| p.rest);
                let t = match closure.filter(|_| !hf.free.is_empty()) {
                    Some(v) => {
                        let o = self.load_place(&PlaceRef::Var(v), Kind::Val);
                        let o = self.coerce(o, Kind::Val);
                        CallTarget::Value(ValSlot(o.r))
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
                (CallTarget::Value(ValSlot(o.r)), c.param_kinds.clone(), false, false)
            }
            Callee::Method { class, vslot } => (
                CallTarget::Method { class: *class, slot: *vslot },
                c.param_kinds.clone(),
                c.rest_at.is_some(),
                true,
            ),
            Callee::Prop(name) => {
                let p = self.prop_site(name);
                (CallTarget::Property(p), c.param_kinds.clone(), false, false)
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
        let mut rest_arr: Option<u16> = None;
        for a in &c.args {
            if let Some(ra) = rest_at
                && i >= ra
            {
                let arr = *rest_arr.get_or_insert_with(|| {
                    let d = self.alloc_v();
                    self.code.push(Instruction::NewArray { output: ValSlot(d), numbers: false });
                    d
                });
                match a {
                    HElem::Expr(e) => {
                        let o = self.expr(e);
                        let o = self.coerce(o, Kind::Val);
                        self.emit(Instruction::PushValue { array: ValSlot(arr), value: ValSlot(o.r) });
                    }
                    HElem::Spread(e) => {
                        let o = self.expr(e);
                        let o = self.coerce(o, Kind::Val);
                        self.emit(Instruction::PushSpread { array: ValSlot(arr), source: ValSlot(o.r) });
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
                self.load_default(o);
                args.push((k, o.r));
                spread.push(false);
            }
            let arr = rest_arr.unwrap_or_else(|| {
                let d = self.alloc_v();
                self.emit(Instruction::NewArray { output: ValSlot(d), numbers: false });
                d
            });
            args.push((Kind::Val, arr));
            spread.push(false);
        }
        let ci = CallSite {
            target,
            this: this.map(ValSlot),
            args,
            has_spread: spread.iter().any(|x| *x),
            spread,
            output: dst.map(|d| (d.k, d.r)),
            packed_rest: rest_at.is_some(),
            region: {
                let key = c as *const HCall as usize;
                match self.pending_regions.iter().rposition(|(k, _)| *k == key) {
                    Some(i) => self.pending_regions.remove(i).1,
                    None => None,
                }
            },
        };
        self.calls.push(ci);
        let ci = self.calls.len() as u32 - 1;
        self.emit(Instruction::Call { site: ci });
    }
}

// ───────────── HIR walkers ─────────────

/// Numeric literals used inside loops (worth a dedicated slot).
fn collect_loop_nums(s: &HStmt, out: &mut Vec<f64>, hir: &HProgram) {
    match s {
        HStmt::Loop { .. } | HStmt::ForOf { .. } | HStmt::ForIn { .. } => collect_nums_stmt(s, out, hir),
        HStmt::Block(ss, _) => ss.iter().for_each(|s| collect_loop_nums(s, out, hir)),
        HStmt::If(_, a, b) => a.iter().chain(b.iter()).for_each(|s| collect_loop_nums(s, out, hir)),
        HStmt::Try { block, handler, finalizer, .. } => {
            block.iter().chain(handler.iter()).for_each(|s| collect_loop_nums(s, out, hir));
            if let Some(f) = finalizer {
                f.iter().for_each(|s| collect_loop_nums(s, out, hir));
            }
        }
        HStmt::Switch { cases, .. } => cases.iter().for_each(|(_, b)| b.iter().for_each(|s| collect_loop_nums(s, out, hir))),
        HStmt::Labeled(_, b) => collect_loop_nums(b, out, hir),
        _ => {}
    }
}

fn collect_nums_stmt(s: &HStmt, out: &mut Vec<f64>, hir: &HProgram) {
    match s {
        HStmt::Expr(e) | HStmt::Throw(e) => collect_nums_expr(e, out, hir),
        HStmt::Init(_, e) | HStmt::Return(e) => {
            if let Some(e) = e {
                collect_nums_expr(e, out, hir)
            }
        }
        HStmt::MakeClosure(..) | HStmt::Break(_) | HStmt::Continue(_) => {}
        HStmt::Block(ss, _) => ss.iter().for_each(|s| collect_nums_stmt(s, out, hir)),
        HStmt::If(c, a, b) => {
            collect_nums_expr(c, out, hir);
            a.iter().chain(b.iter()).for_each(|s| collect_nums_stmt(s, out, hir));
        }
        HStmt::Loop { test, body, update, .. } => {
            if let Some(t) = test {
                collect_nums_expr(t, out, hir);
            }
            if let Some(u) = update {
                collect_nums_expr(u, out, hir);
            }
            body.iter().for_each(|s| collect_nums_stmt(s, out, hir));
        }
        HStmt::ForOf { iter, body, .. } | HStmt::ForIn { obj: iter, body, .. } => {
            collect_nums_expr(iter, out, hir);
            body.iter().for_each(|s| collect_nums_stmt(s, out, hir));
        }
        HStmt::Try { block, handler, finalizer, .. } => {
            block.iter().chain(handler.iter()).for_each(|s| collect_nums_stmt(s, out, hir));
            if let Some(f) = finalizer {
                f.iter().for_each(|s| collect_nums_stmt(s, out, hir));
            }
        }
        HStmt::Switch { disc, cases, .. } => {
            collect_nums_expr(disc, out, hir);
            for (t, b) in cases {
                if let Some(t) = t {
                    collect_nums_expr(t, out, hir);
                }
                b.iter().for_each(|s| collect_nums_stmt(s, out, hir));
            }
        }
        HStmt::Labeled(_, b) => collect_nums_stmt(b, out, hir),
    }
}

fn collect_nums_place(p: &Place, out: &mut Vec<f64>, hir: &HProgram) {
    match p {
        Place::Var(_) => {}
        Place::Field { obj, .. }
        | Place::Prop { obj, .. }
        | Place::ArrLen { obj, .. }
        | Place::StrLen { obj, .. }
        | Place::Accessor { obj, .. } => {
            collect_nums_expr(obj, out, hir)
        }
        Place::Index { obj, index: k, .. } | Place::DynIndex { obj, key: k, .. } => {
            collect_nums_expr(obj, out, hir);
            collect_nums_expr(k, out, hir);
        }
    }
}

fn collect_nums_elems(es: &[HElem], out: &mut Vec<f64>, hir: &HProgram) {
    for e in es {
        match e {
            HElem::Expr(x) | HElem::Spread(x) => collect_nums_expr(x, out, hir),
        }
    }
}

fn collect_nums_expr(e: &HExpr, out: &mut Vec<f64>, hir: &HProgram) {
    match &e.kind {
        HK::Num(v) => {
            if !out.iter().any(|x| x.to_bits() == v.to_bits()) {
                out.push(*v)
            }
        }
        HK::Read(p) => collect_nums_place(p, out, hir),
        HK::Array(es) | HK::New { args: es, .. } | HK::NewBuiltin(_, es) => collect_nums_elems(es, out, hir),
        HK::Object(ps) => {
            for p in ps {
                match p {
                    HProp::Init(_, x) | HProp::Spread(x) | HProp::Computed(_, x) => collect_nums_expr(x, out, hir),
                    HProp::Accessor { .. } => {}
                }
            }
        }
        HK::StmtExpr(_, x) | HK::Unary(_, x) | HK::Cast(x) | HK::OptChain(x) | HK::InstanceOf(x, _) | HK::Await(x) | HK::Yield(x) | HK::YieldStar(x) => {
            collect_nums_expr(x, out, hir)
        }
        HK::Binary(_, a, b) | HK::Logical(_, a, b) | HK::Delete(a, b) => {
            collect_nums_expr(a, out, hir);
            collect_nums_expr(b, out, hir);
        }
        HK::Cond(a, b, c) => {
            collect_nums_expr(a, out, hir);
            collect_nums_expr(b, out, hir);
            collect_nums_expr(c, out, hir);
        }
        HK::Assign(p, v) | HK::Compound(p, _, v) | HK::LogicalAssign(p, _, v) => {
            collect_nums_place(p, out, hir);
            collect_nums_expr(v, out, hir);
        }
        HK::Update { place, .. } => collect_nums_place(place, out, hir),
        HK::Call(c) => {
            if let Callee::Value(f) = &c.callee {
                collect_nums_expr(f, out, hir);
            }
            if let Some(t) = &c.this {
                collect_nums_expr(t, out, hir);
            }
            collect_nums_elems(&c.args, out, hir);
        }
        HK::Seq(es) => es.iter().for_each(|x| collect_nums_expr(x, out, hir)),
        // Closure bodies may be inlined into this frame.
        HK::Closure(f) => {
            for st in &hir.funcs[*f as usize].body {
                collect_nums_stmt(st, out, hir);
            }
        }
        _ => {}
    }
}

/// Element kind of `arr.map(f)`'s result, from the callback's static return type.
fn c_ret_elem(args: &[HElem], n: Native, _elem: &Type) -> Option<Kind> {
    if n != Native::ArrMap {
        return None;
    }
    match args.first() {
        Some(HElem::Expr(e)) => match &e.ty {
            Type::Func(ft) => Some(ft.ret.kind()),
            _ => None,
        },
        _ => None,
    }
}

fn jump_target(i: &mut Instruction) -> Option<&mut u32> {
    match i {
        Instruction::Jump { to: t }
        | Instruction::JumpIf { to: t, .. }
        | Instruction::JumpIfCompare { to: t, .. }
        | Instruction::JumpIfTruthy { to: t, .. }
        | Instruction::JumpIfNullish { to: t, .. }
        | Instruction::IteratorStep { to: t, .. }
        | Instruction::UnpackIteratorResult { to: t, .. } => Some(t),
        Instruction::TryBegin { catch, .. } => Some(catch),
        _ => None,
    }
}

/// Remove unconditional jumps to the next instruction and thread jumps to jumps.
/// Returns the old → new instruction index map when instructions were removed.
fn peephole(code: &mut Vec<Instruction>) -> Option<Vec<u32>> {
    // Thread: a jump whose target is an unconditional jump goes straight to its target.
    for i in 0..code.len() {
        let mut ins = code[i];
        if let Some(t) = jump_target(&mut ins) {
            let mut tgt = *t;
            let mut hops = 0;
            while let Instruction::Jump { to: next } = code[tgt as usize] {
                if next == tgt || hops > 8 {
                    break;
                }
                tgt = next;
                hops += 1;
            }
            *t = tgt;
            code[i] = ins;
        }
    }
    let keep: Vec<bool> = code
        .iter()
        .enumerate()
        .map(|(i, ins)| !matches!(ins, Instruction::Jump { to: t } if *t as usize == i + 1))
        .collect();
    if keep.iter().all(|k| *k) {
        return None;
    }
    let mut new_index = Vec::with_capacity(code.len() + 1);
    let mut n = 0u32;
    for k in &keep {
        new_index.push(n);
        if *k {
            n += 1;
        }
    }
    new_index.push(n);
    let mut out = Vec::with_capacity(n as usize);
    for (i, mut ins) in code.drain(..).enumerate() {
        if !keep[i] {
            continue;
        }
        if let Some(t) = jump_target(&mut ins) {
            *t = new_index[*t as usize];
        }
        out.push(ins);
    }
    *code = out;
    Some(new_index)
}
