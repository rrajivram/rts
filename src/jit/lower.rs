//! Lowering HIR to Cranelift IR (`Lower`), and `compile`, which drives it.

use super::*;

// ───────────── lowering ─────────────

pub(super) fn cl_ty(k: JK) -> types::Type {
    match k {
        JK::Num => types::F64,
        JK::Bool => types::I8,
        JK::Ref | JK::Val => types::I64,
    }
}

/// The machine words of a `k`: a value takes two (tag, payload).
pub(super) fn cl_tys(k: JK) -> Vec<types::Type> {
    if k.wide() { vec![types::I64, types::I64] } else { vec![cl_ty(k)] }
}

/// The runtime functions compiled code calls, each declared once: the
/// field compiled code uses, the Rust function, and its signature.
macro_rules! helpers {
    ($($field:ident = $func:ident($($p:ident),*) $(-> $r:ident)?;)*) => {
        struct Helpers {
            $($field: FuncRef,)*
        }

        impl Helpers {
            /// (symbol, address, parameter types, result type) of each helper.
            fn table() -> Vec<(&'static str, *const u8, Vec<types::Type>, Option<types::Type>)> {
                vec![$((concat!("rts_", stringify!($field)), $func as *const u8, vec![$(types::$p),*], helpers!(@ret $($r)?)),)*]
            }

            /// The helpers as referenced from one function (`refs` in `table` order).
            fn new(refs: &[FuncRef]) -> Helpers {
                let mut it = refs.iter().copied();
                Helpers { $($field: it.next().unwrap(),)* }
            }
        }
    };
    (@ret) => { None };
    (@ret $r:ident) => { Some(types::$r) };
}

helpers! {
    modf = h_mod(F64, F64) -> F64;
    pow = h_pow(F64, F64) -> F64;
    int32 = h_int32(F64) -> I32;
    math1 = h_math1(I32, F64) -> F64;
    math2 = h_math2(I32, F64, F64) -> F64;
    new = h_new(I64, I64) -> I64;
    dec = h_dec(I64);
    drop_val = h_drop_val(I64, I64);
    throw = h_throw(I32, I64, I64);
    arr_get = h_arr_get(I64, F64, I32) -> I64;
    arr_set = h_arr_set(I64, F64, I64, I64, I32);
    arr_len = h_arr_len(I64) -> F64;
    arr_push = h_arr_push(I64, I64, I64) -> F64;
    arr_new = h_arr_new(I32, I64) -> I64;
    throw_site = h_throw_site(I32);
    inc_val = h_inc_val(I64, I64);
    call = h_call(I32, I64, I64, I64, I64, I64, I32) -> I64;
    call_named = h_call_named(I64, I64, I64, I64, I64, I32) -> I64;
    get_prop = h_get_prop(I64, I64, I64) -> I64;
    set_prop = h_set_prop(I64, I64, I64, I64, I64);
    op = h_op(I32, I64, I64, I64, I64) -> I64;
    dyn_op = h_dyn(I32, I64, I64, I64, I64) -> I64;
    get_dyn = h_get_dyn(I64, I64, I64, I64) -> I64;
    set_dyn = h_set_dyn(I64, I64, I64, I64, I64, I64);
    new_cell = h_new_cell() -> I64;
    closure = h_closure(I32, I64, I32) -> I64;
    throw_value = h_throw_value(I64, I64);
    iter_array = h_iter_array(I64, I64) -> I64;
    region_enter = h_region_enter() -> I64;
    region_exit = h_region_exit();
    gc_poll = h_gc_poll();
    region_unwind = h_region_unwind(I64);
}

/// Layout facts compiled code relies on.
pub(super) struct Layout {
    tag_undef: i64,
    tag_null: i64,
    tag_bool: i64,
    tag_num: i64,
    tag_obj: i64,
    tag_arr: i64,
    tag_func: i64,
    /// Tags from here on (`BigInt`, …) may own a box.
    tag_first_gc: i64,
    /// Where a cell's value is, from the start of its box.
    cell_value: i32,
    /// A closure's function id (u32) and its cells' data pointer, from the
    /// start of its box (None: the cells' pointer couldn't be found).
    clo_func: i32,
    clo_caps: Option<i32>,
    /// A string's bytes pointer and length (bytes), from the start of its box.
    str_bytes: Option<(i32, i32)>,
    tag_str: i64,
    arr: Option<ArrLayout>,
    /// An object's class pointer, from the bits of its shape's `Rc`.
    shape_class: i32,
}

impl Layout {
    fn new() -> Layout {
        let t = |v: Value| v.tag() as i64;
        let l = Layout {
            tag_undef: t(Value::Undef),
            tag_null: t(Value::Null),
            tag_bool: t(Value::Bool(false)),
            tag_num: t(Value::Num(0.0)),
            tag_obj: t(Value::Obj(Object::new(crate::runtime::Shape::new(vec![]), vec![]))),
            tag_arr: t(Value::Arr(crate::runtime::Array::new_num(vec![]))),
            tag_func: t(Value::Func(Gc::new(Closure { func: 0, caps: Box::new([]), props: Default::default() }))),
            tag_first_gc: t(Value::BigInt(Gc::new(0))),
            cell_value: {
                let c = Gc::new(RefCell::new(Value::Undef));
                (c.as_ptr() as usize - c.header_ptr().as_ptr() as usize) as i32
            },
            arr: if std::env::var_os("RTS_JIT_NO_INLINE_ARRAYS").is_some() { None } else { ArrLayout::probe() },
            str_bytes: {
                let st = crate::runtime::JsStr::new("abc");
                let at = Gc::<crate::runtime::JsStr>::VALUE_OFFSET + std::mem::offset_of!(crate::runtime::JsStr, s);
                let w: [usize; 2] = unsafe { std::mem::transmute_copy(&st.s) };
                w.iter()
                    .position(|x| *x == st.s.as_ptr() as usize)
                    .filter(|&i| w[1 - i] == 3)
                    .map(|i| ((at + 8 * i) as i32, (at + 8 * (1 - i)) as i32))
            },
            tag_str: t(Value::str("")),
            shape_class: {
                let s = crate::runtime::Shape::new(vec![]);
                let bits: usize = unsafe { std::mem::transmute_copy(&s) };
                (std::rc::Rc::as_ptr(&s) as usize - bits + crate::runtime::Shape::CLASS_OFFSET) as i32
            },
            clo_func: (Gc::<Closure>::VALUE_OFFSET + std::mem::offset_of!(Closure, func)) as i32,
            clo_caps: {
                // `Box<[T]>` is (pointer, length) in some order: find the pointer.
                let c = Gc::new(Closure { func: 0, caps: Box::new([Gc::new(RefCell::new(Value::Undef))]), props: Default::default() });
                let at = std::mem::offset_of!(Closure, caps);
                let w: [usize; 2] = unsafe { std::mem::transmute_copy(&c.caps) };
                w.iter().position(|x| *x == c.caps.as_ptr() as usize).filter(|&i| w[1 - i] == 1).map(|i| (Gc::<Closure>::VALUE_OFFSET + at + 8 * i) as i32)
            },
        };
        assert!(l.tag_undef < l.tag_first_gc && l.tag_null < l.tag_first_gc && l.tag_bool < l.tag_first_gc && l.tag_num < l.tag_first_gc);
        l
    }
}

/// Where an array's elements are, as offsets from its box (`None`: unknown,
/// so every access calls a helper). `Vec`'s field order is not fixed by
/// Rust, so it is found by inspecting a vector.
#[derive(Clone, Copy)]
pub(super) struct ArrLayout {
    /// The `ArrStore` variant byte (0: numbers, 1: values).
    variant: i32,
    data: i32,
    len: i32,
}

impl ArrLayout {
    fn probe() -> Option<ArrLayout> {
        use crate::runtime::{ArrStore, Array};
        const W: usize = std::mem::size_of::<usize>();
        if std::mem::size_of::<Vec<f64>>() != 3 * W || std::mem::size_of::<Vec<Value>>() != 3 * W {
            return None;
        }
        // Which word of a vector is the data pointer, and which the length?
        let words = |p: usize, len: usize, cap: usize, w: [usize; 3]| -> Option<(usize, usize)> {
            let pi = w.iter().position(|x| *x == p)?;
            let li = w.iter().position(|x| *x == len)?;
            let ci = w.iter().position(|x| *x == cap)?;
            (pi != li && li != ci && pi != ci).then_some((pi, li))
        };
        let mut v: Vec<f64> = Vec::with_capacity(7);
        v.extend([1.0, 2.0, 3.0]);
        let (p, l, c) = (v.as_ptr() as usize, v.len(), v.capacity());
        let w: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        let num = words(p, l, c, w)?;
        let mut v: Vec<Value> = Vec::with_capacity(5);
        v.extend([Value::Undef, Value::Null]);
        let (p, l, c) = (v.as_ptr() as usize, v.len(), v.capacity());
        let w: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        if words(p, l, c, w)? != num {
            return None;
        }
        // The store inside the array's box.
        let a = Array::new_num(vec![1.0]);
        let base = a.header_ptr().as_ptr() as usize;
        let store = a.store.as_ptr() as usize - base;
        // The variant byte and the vector's offset (`repr(C, u8)`).
        let tag = |s: &ArrStore| unsafe { *(s as *const ArrStore as *const u8) };
        if tag(&ArrStore::Num(vec![])) != 0 || tag(&ArrStore::Val(vec![])) != 1 {
            return None;
        }
        let vec_at = store + 8;
        Some(ArrLayout { variant: store as i32, data: (vec_at + num.0 * W) as i32, len: (vec_at + num.1 * W) as i32 })
    }
}

/// Static counts of the reference-count updates compiled code makes and
/// skips (`RTS_JIT_DEBUG`).
#[derive(Default)]
pub(super) struct RcStats {
    incs: usize,
    decs: usize,
    borrowed: usize,
}

pub(super) struct Lower<'a, 'b> {
    hir: &'a HProgram,
    el: &'a Elig<'a>,
    fid: FuncId,
    rk: JK,
    /// The result, in detail.
    rjv: JV,
    b: FunctionBuilder<'b>,
    /// Each variable's word (a value's payload).
    vars: FxHashMap<VarId, Variable>,
    /// A value variable's tag word.
    tvars: FxHashMap<VarId, Variable>,
    /// Captured variables this function declares: each holds its cell (a
    /// counted box address, or 1 before the cell is made).
    cells: FxHashMap<VarId, Variable>,
    /// Cells declared by a block this code has entered (see `HStmt::Init`).
    declared: FxHashSet<VarId>,
    /// The cells this function's closure captured (its `free` variables, in order).
    env: Option<CV>,
    /// Counted variables holding a counted reference (locals, and assigned parameters).
    owned: Vec<VarId>,
    /// Reference variables that hold borrowed values (see `Elig::borrowed_vars`).
    borrowed: FxHashSet<VarId>,
    ctx: CV,
    depth: CV,
    unwind: Block,
    /// Owned temporaries not yet consumed (payload, tag): released if the
    /// code unwinds.
    pending: Vec<(CV, Option<CV>)>,
    loops: Vec<LoopT>,
    /// The escape analysis (allocation sites → scope regions).
    esc: &'a crate::escape::EscapeInfo,
    /// Regions entered and not yet left, innermost last: the block's key,
    /// its arena's address, and what to release before freeing it.
    regions: Vec<RegionT>,
    /// The arena the caller gave for this function's result (0: the heap),
    /// when the function has allocations only ever returned.
    rarena: Option<CV>,
    /// The JIT region stack's height on entry (functions with regions), for unwinding.
    region_mark: Option<CV>,
    /// Compiled functions that take a result arena.
    takes_rarena: &'a FxHashSet<FuncId>,
    /// Each `for-of`'s array (keyed by the statement's address): owned,
    /// or 1 when the loop isn't running.
    iters: FxHashMap<usize, Variable>,
    /// Compiled functions this one calls, imported on first use.
    funcs: FxHashMap<FuncId, FuncRef>,
    /// Every function called directly (it must be defined with this one).
    called: &'a RefCell<Vec<FuncId>>,
    /// Functions already compiled (or being compiled in this batch).
    defined: &'a FxHashSet<FuncId>,
    /// Every compiled function's id and signature (for importing it).
    fdecls: &'a FxHashMap<FuncId, (ClFuncId, Signature)>,
    /// Calls through function values: the kinds keys, and the table of
    /// entries by function id (see `NativeEntry`).
    keys: &'a RefCell<FxHashMap<(Vec<JV>, JV), u32>>,
    entries: *const NativeEntry,
    /// An empty signature with the module's calling convention.
    sig0: &'a Signature,
    h: Helpers,
    lay: &'a Layout,
    classes: &'a [std::rc::Rc<ClassRt>],
    /// Each global variable's index (in `Vm::global_scalars` or `global_values`, by kind).
    globals: &'a FxHashMap<VarId, u32>,
    stats: &'a mut RcStats,
}

/// The place of a numeric `+=` or `++` (see `Lower::num_place`).
pub(super) enum NumPlace {
    Var(VarId),
    Field(RV, u32),
    Index(RV, CV),
}

/// A `break` / `continue` target.
#[derive(Clone)]
pub(super) struct LoopT {
    label: Option<Name>,
    brk: Block,
    cont: Option<Block>,
    switch: bool,
    /// Regions active outside it (left when jumping out).
    regions: usize,
}

/// An entered scope region (see `escape.rs`).
pub(super) struct RegionT {
    key: usize,
    arena: CV,
    /// Variables declared in the block (counted, owned) and `for-of`
    /// arrays: released before the arena is freed.
    vars: Vec<VarId>,
    iters: Vec<usize>,
}

/// A value in compiled code: one word (a number, boolean or reference), or
/// two for any value (`t`, its tag, and `v`, its payload). For a counted
/// kind, `owned` says whether this code holds a count on it.
#[derive(Clone, Copy)]
pub(super) struct RV {
    v: CV,
    t: Option<CV>,
    owned: bool,
}

impl RV {
    fn new(v: CV) -> RV {
        RV { v, t: None, owned: false }
    }
    fn val(t: CV, v: CV) -> RV {
        RV { v, t: Some(t), owned: false }
    }
    fn words(self) -> Vec<CV> {
        match self.t {
            Some(t) => vec![t, self.v],
            None => vec![self.v],
        }
    }
    fn of_words(w: &[CV]) -> RV {
        match w {
            [t, v] => RV::val(*t, *v),
            [v] => RV::new(*v),
            _ => unreachable!(),
        }
    }
}

pub(super) fn mf() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// `JitCtx` field offsets.
pub(super) const CTX_RET_TAG: i32 = std::mem::offset_of!(JitCtx, ret_tag) as i32;
pub(super) const CTX_GLOBAL_SCALARS: i32 = std::mem::offset_of!(JitCtx, global_scalars) as i32;
pub(super) const CTX_GLOBAL_VALUES: i32 = std::mem::offset_of!(JitCtx, global_values) as i32;

impl Lower<'_, '_> {
    fn var(&mut self, v: VarId) -> Variable {
        if let Some(x) = self.vars.get(&v) {
            return *x;
        }
        let k = self.el.vjk(v).expect("compiled variable");
        let x = self.b.declare_var(cl_ty(k));
        let init = self.zero_of(k);
        self.b.def_var(x, init);
        self.vars.insert(v, x);
        if k.wide() {
            let t = self.b.declare_var(types::I64);
            let u = self.b.ins().iconst(types::I64, self.lay.tag_undef);
            self.b.def_var(t, u);
            self.tvars.insert(v, t);
        }
        x
    }

    /// A variable's value (borrowed).
    fn use_var(&mut self, v: VarId) -> RV {
        let x = self.var(v);
        let p = self.b.use_var(x);
        match self.tvars.get(&v).copied() {
            Some(t) => RV::val(self.b.use_var(t), p),
            None => RV::new(p),
        }
    }

    /// A global variable's value (borrowed).
    fn global_get(&mut self, v: VarId) -> RV {
        let i = self.globals[&v] as i32;
        let vj = var_jv(self.hir, v);
        match vj {
            JV::Num | JV::Bool => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_SCALARS);
                let w = self.b.ins().load(types::I64, mf(), base, i * 8);
                RV::new(if vj == JV::Num { self.b.ins().bitcast(types::F64, MemFlagsData::new(), w) } else { self.b.ins().ireduce(types::I8, w) })
            }
            _ => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_VALUES);
                self.load_slot(base, i * 16, vj, [ERR_UNBOX_NUM, ERR_UNBOX_BOOL, ERR_UNBOX_REF])
            }
        }
    }

    /// Store into a global variable (`r` owned, if counted), releasing the old value.
    #[track_caller]
    fn global_set(&mut self, v: VarId, r: RV) {
        let i = self.globals[&v] as i32;
        let vj = var_jv(self.hir, v);
        match vj {
            JV::Num | JV::Bool => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_SCALARS);
                let w = if vj == JV::Num { self.b.ins().bitcast(types::I64, MemFlagsData::new(), r.v) } else { self.b.ins().uextend(types::I64, r.v) };
                self.b.ins().store(mf(), w, base, i * 8);
            }
            _ => {
                let base = self.b.ins().load(types::I64, mf(), self.ctx, CTX_GLOBAL_VALUES);
                self.store_slot(base, i * 16, vj, r);
            }
        }
    }

    fn is_global(&self, v: VarId) -> bool {
        self.hir.vars[v as usize].owner.is_none()
    }

    /// A variable in memory (a global, or captured: in a cell), not a register.
    fn in_mem(&self, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner.is_none() || vi.captured
    }

    /// The address of a captured variable's cell (borrowed).
    fn cell_ptr(&mut self, v: VarId) -> CV {
        if let Some(x) = self.cells.get(&v).copied() {
            return self.b.use_var(x);
        }
        let i = self.hir.funcs[self.fid as usize].free.iter().position(|x| *x == v).expect("a captured variable");
        let env = self.env.expect("a closure's cells");
        self.b.ins().load(types::I64, mf(), env, (i * 8) as i32)
    }

    /// A variable's value (borrowed: a global's or cell's may change in any call).
    fn read_var(&mut self, v: VarId) -> RV {
        if self.is_global(v) {
            self.global_get(v)
        } else if self.in_mem(v) {
            let c = self.cell_ptr(v);
            let off = self.lay.cell_value;
            self.load_slot(c, off, var_jv(self.hir, v), [ERR_UNBOX_NUM, ERR_UNBOX_BOOL, ERR_UNBOX_REF])
        } else {
            self.use_var(v)
        }
    }

    /// Store into a variable wherever it lives (`r` owned, if counted and
    /// the variable counts), releasing the old value.
    #[track_caller]
    fn write_var(&mut self, v: VarId, r: RV) {
        if self.in_mem(v) { self.mem_set(v, r) } else { self.set_var(v, r) }
    }

    /// Store into a global or captured variable (`r` owned, if counted), releasing the old value.
    #[track_caller]
    fn mem_set(&mut self, v: VarId, r: RV) {
        if self.is_global(v) {
            self.global_set(v, r);
        } else {
            let c = self.cell_ptr(v);
            let off = self.lay.cell_value;
            self.store_slot(c, off, var_jv(self.hir, v), r);
        }
    }

    /// Give captured variable `v` (one this function declares) a new cell,
    /// holding undefined, releasing the old one.
    #[track_caller]
    fn new_cell(&mut self, v: VarId) {
        let x = self.cells[&v];
        let old = self.b.use_var(x);
        let call = self.b.ins().call(self.h.new_cell, &[]);
        let c = self.b.inst_results(call)[0];
        self.b.def_var(x, c);
        self.dec(old);
    }

    /// A new closure of `g` (owned), capturing its cells from this function.
    fn make_closure(&mut self, g: FuncId) -> RV {
        use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
        let free = self.hir.funcs[g as usize].free.clone();
        let n = free.len();
        let cells = if n > 0 {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (8 * n) as u32, 3));
            self.b.ins().stack_addr(types::I64, slot, 0)
        } else {
            self.b.ins().iconst(types::I64, 0)
        };
        for (i, v) in free.iter().enumerate() {
            let c = self.cell_ptr(*v);
            self.b.ins().store(mf(), c, cells, (8 * i) as i32);
        }
        let gi = self.b.ins().iconst(types::I32, g as i64);
        let ni = self.b.ins().iconst(types::I32, n as i64);
        let call = self.b.ins().call(self.h.closure, &[gi, cells, ni]);
        let p = self.b.inst_results(call)[0];
        let t = self.b.ins().iconst(types::I64, self.lay.tag_func);
        self.fresh(RV::val(t, p))
    }

    fn def_var(&mut self, v: VarId, r: RV) {
        let x = self.var(v);
        self.b.def_var(x, r.v);
        if let Some(t) = self.tvars.get(&v).copied() {
            self.b.def_var(t, r.t.expect("a value"));
        }
    }

    /// `undefined` (NaN, false) as a `k`'s single or payload word.
    fn zero_of(&mut self, k: JK) -> CV {
        match k {
            JK::Num => self.b.ins().f64const(f64::NAN),
            JK::Bool => self.b.ins().iconst(types::I8, 0),
            JK::Ref => self.b.ins().iconst(types::I64, 1),
            JK::Val => self.b.ins().iconst(types::I64, 0),
        }
    }

    fn zero_rv(&mut self, k: JK) -> RV {
        let v = self.zero_of(k);
        if k.wide() {
            let t = self.b.ins().iconst(types::I64, self.lay.tag_undef);
            RV::val(t, v)
        } else {
            RV::new(v)
        }
    }

    /// Code after a jump, return or break is unreachable: give it a block of its own.
    fn dead_block(&mut self) {
        let blk = self.b.create_block();
        self.b.switch_to_block(blk);
        self.b.seal_block(blk);
    }

    /// Branch to a new block if `cond`, run `then` there, and continue after.
    fn if_then(&mut self, cond: CV, then: impl FnOnce(&mut Self)) {
        let (tb, done) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(cond, tb, &[], done, &[]);
        self.b.seal_block(tb);
        self.b.switch_to_block(tb);
        then(self);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    // ── reference counts ──

    /// Profile builds: count an inline update of box `p` (and whether the
    /// box is in a scope arena).
    #[cfg(feature = "profile")]
    fn count_rc(&mut self, p: CV) {
        let a = self.b.ins().iconst(types::I64, heap::STATS.rc_ops.as_ptr() as i64);
        let n = self.b.ins().load(types::I64, mf(), a, 0);
        let n1 = self.b.ins().iadd_imm_s(n, 1);
        self.b.ins().store(mf(), n1, a, 0);
        let fl = self.b.ins().load(types::I32, mf(), p, 4);
        let bit = self.b.ins().ushr_imm_u(fl, heap::flag::ARENA.trailing_zeros() as i64);
        let bit = self.b.ins().band_imm_u(bit, 1);
        let bit = self.b.ins().uextend(types::I64, bit);
        let a = self.b.ins().iconst(types::I64, (&raw mut ARENA_RC) as i64);
        let n = self.b.ins().load(types::I64, mf(), a, 0);
        let n1 = self.b.ins().iadd(n, bit);
        self.b.ins().store(mf(), n1, a, 0);
    }
    #[cfg(not(feature = "profile"))]
    fn count_rc(&mut self, _p: CV) {}

    /// Profile builds: count a count update at the lower.rs line that asked
    /// for it (see `rc_sites_report`).
    #[cfg(feature = "profile")]
    fn count_site(&mut self, loc: &'static std::panic::Location<'static>, inc: bool) {
        let i = RC_SITES.with(|t| {
            let mut t = t.borrow_mut();
            let key = (self.fid, loc.line(), inc);
            match t.iter().position(|k| *k == key) {
                Some(i) => i,
                None => {
                    t.push(key);
                    t.len() - 1
                }
            }
        });
        if i >= RC_SITE_MAX {
            return;
        }
        let a = self.b.ins().iconst(types::I64, (&raw mut RC_SITE_COUNTS) as *mut u64 as i64 + 8 * i as i64);
        let n = self.b.ins().load(types::I64, mf(), a, 0);
        let n1 = self.b.ins().iadd_imm_s(n, 1);
        self.b.ins().store(mf(), n1, a, 0);
    }
    #[cfg(not(feature = "profile"))]
    fn count_site(&mut self, _loc: &'static std::panic::Location<'static>, _inc: bool) {}

    /// Increment a box's count, inline.
    #[track_caller]
    fn inc_box(&mut self, p: CV) {
        let rc = self.b.ins().load(types::I32, mf(), p, 0);
        let rc1 = self.b.ins().iadd_imm_s(rc, 1);
        self.b.ins().store(mf(), rc1, p, 0);
        self.count_rc(p);
    }

    /// Take a reference (`p` may be null or undefined).
    #[track_caller]
    fn inc(&mut self, p: CV) {
        self.stats.incs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let loc = std::panic::Location::caller();
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, p, 2);
        self.if_then(real, |s| {
            s.count_site(loc, true);
            s.inc_box(p)
        });
    }

    /// Take a reference to a value given as words: inline for an object or
    /// array, through a helper for other boxed kinds.
    #[track_caller]
    fn inc_val(&mut self, t: CV, p: CV) {
        self.val_count(t, p, true);
    }

    /// Release a reference (`p` may be null or undefined).
    #[track_caller]
    fn dec(&mut self, p: CV) {
        self.stats.decs += 1;
        if cfg!(feature = "gc") {
            return;
        }
        let loc = std::panic::Location::caller();
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, p, 2);
        self.if_then(real, |s| {
            s.count_site(loc, false);
            s.dec_box(p)
        });
    }

    #[track_caller]
    fn inc_rv(&mut self, r: RV) {
        match r.t {
            Some(t) => self.inc_val(t, r.v),
            None => self.inc(r.v),
        }
    }

    #[track_caller]
    fn dec_rv(&mut self, r: RV) {
        match r.t {
            Some(t) => self.release_old(t, r.v),
            None => self.dec(r.v),
        }
    }

    /// Release a reference to a box: inline unless the count may reach zero
    /// or the box may become a cycle candidate.
    #[track_caller]
    fn dec_box(&mut self, p: CV) {
        let rc = self.b.ins().load(types::I32, mf(), p, 0);
        let fl = self.b.ins().load(types::I32, mf(), p, 4);
        let mask = (heap::flag::ACYCLIC | heap::flag::BUFFERED | heap::flag::ARENA) as i64;
        let m = self.b.ins().band_imm_u(fl, mask);
        let quiet = self.b.ins().icmp_imm_u(IntCC::NotEqual, m, 0);
        let above1 = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, rc, 1);
        let fast = self.b.ins().band(quiet, above1);
        let (fb, sb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(fast, fb, &[], sb, &[]);
        self.b.seal_block(fb);
        self.b.seal_block(sb);
        self.b.switch_to_block(fb);
        let rc1 = self.b.ins().iadd_imm_s(rc, -1);
        self.b.ins().store(mf(), rc1, p, 0);
        self.count_rc(p);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(sb);
        self.b.ins().call(self.h.dec, &[p]);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    /// A new owned temporary.
    fn fresh(&mut self, r: RV) -> RV {
        self.pending.push((r.v, r.t));
        RV { owned: true, ..r }
    }

    fn unpend(&mut self, r: RV) {
        let i = self.pending.iter().rposition(|p| *p == (r.v, r.t)).expect("pending temporary");
        self.pending.remove(i);
    }

    /// `old` became `new` (a conversion): a pending temporary follows.
    fn repend(&mut self, old: RV, new: RV) {
        if !old.owned {
            return;
        }
        if let Some(i) = self.pending.iter().rposition(|p| *p == (old.v, old.t)) {
            if new.owned {
                self.pending[i] = (new.v, new.t);
            } else {
                self.pending.remove(i);
            }
        }
    }

    /// An owned copy of `r`, handed to the caller (who stores it at once).
    #[track_caller]
    fn own(&mut self, r: RV) -> RV {
        if r.owned {
            self.unpend(r);
        } else {
            self.inc_rv(r);
        }
        RV { owned: true, ..r }
    }

    /// `r`, owned while other code runs.
    #[track_caller]
    fn hold(&mut self, r: RV) -> RV {
        // Deferred counting: the stack reference keeps it (see `zct`).
        if r.owned || crate::runtime::zct::deferred() {
            return r;
        }
        self.inc_rv(r);
        self.fresh(r)
    }

    /// `r` without a count of its own (a borrowed variable's value): an
    /// owned temporary is released now. Only deferred counting borrows
    /// owned values; otherwise they are borrowed already.
    #[track_caller]
    fn disown(&mut self, r: RV) -> RV {
        debug_assert!(!r.owned || crate::runtime::zct::deferred());
        if r.owned {
            self.unpend(r);
            self.dec_rv(r);
        }
        RV { owned: false, ..r }
    }

    /// Done with `r`.
    #[track_caller]
    fn release(&mut self, r: RV) {
        if r.owned {
            self.unpend(r);
            self.dec_rv(r);
        } else {
            self.stats.borrowed += 1;
        }
    }

    /// Leave for the unwind block, releasing the pending temporaries.
    #[track_caller]
    fn bail(&mut self) {
        for (v, t) in self.pending.clone() {
            self.dec_rv(RV { v, t, owned: true });
        }
        self.b.ins().jump(self.unwind, &[]);
    }

    /// Release every owned variable (except `keep`) and cell.
    #[track_caller]
    fn release_vars(&mut self, keep: Option<VarId>) {
        for v in self.owned.clone() {
            if Some(v) != keep {
                let r = self.use_var(v);
                self.dec_rv(r);
            }
        }
        let mut cells: Vec<(VarId, Variable)> = self.cells.iter().map(|(v, x)| (*v, *x)).collect();
        cells.sort_by_key(|c| c.0);
        for (_, x) in cells {
            let c = self.b.use_var(x);
            self.dec(c);
        }
        let mut iters: Vec<(usize, Variable)> = self.iters.iter().map(|(k, x)| (*k, *x)).collect();
        iters.sort_by_key(|c| c.0);
        for (_, x) in iters {
            let a = self.b.use_var(x);
            self.dec(a);
        }
    }

    fn ret(&mut self, r: RV) {
        self.b.ins().store(mf(), self.depth, self.ctx, 0);
        self.b.ins().return_(&r.words());
    }

    /// Unwind if a callee or helper set the status.
    fn check_status(&mut self) {
        let st = self.b.ins().load(types::I32, mf(), self.ctx, 4);
        if self.pending.is_empty() {
            let ok = self.b.create_block();
            self.b.ins().brif(st, self.unwind, &[], ok, &[]);
            self.b.seal_block(ok);
            self.b.switch_to_block(ok);
            return;
        }
        let (bad, ok) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(st, bad, &[], ok, &[]);
        self.b.seal_block(bad);
        self.b.seal_block(ok);
        self.b.switch_to_block(bad);
        self.bail();
        self.b.switch_to_block(ok);
    }

    /// Throw a type error (unless `cond` is false) and unwind.
    fn throw_if(&mut self, cond: CV, code: u32, a: CV, b: CV) {
        let (tb, ok) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(cond, tb, &[], ok, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(ok);
        self.b.switch_to_block(tb);
        let c = self.b.ins().iconst(types::I32, code as i64);
        self.b.ins().call(self.h.throw, &[c, a, b]);
        self.bail();
        self.b.switch_to_block(ok);
    }

    /// Check that `p` is an object (not null or undefined).
    fn check_obj(&mut self, p: CV, code: u32) {
        let nullish = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, p, 2);
        let z = self.b.ins().iconst(types::I64, 0);
        self.throw_if(nullish, code, p, z);
    }

    // ── statements ──

    fn stmts(&mut self, ss: &[HStmt]) {
        for s in ss {
            self.stmt(s);
            debug_assert!(self.pending.is_empty(), "temporaries outlive a statement");
        }
    }

    fn stmt(&mut self, s: &HStmt) {
        match s {
            HStmt::Expr(e) => self.discard(e),
            HStmt::Init(v, e) => {
                // A captured variable gets its cell here unless its block made one (as codegen does).
                if self.cells.contains_key(v) && !self.declared.contains(v) {
                    self.new_cell(*v);
                }
                let vj = var_jv(self.hir, *v);
                let val = match e {
                    Some(e) if self.borrowed.contains(v) => {
                        let r = self.value_as(e, vj);
                        let r = self.disown(r);
                        self.stats.borrowed += 1;
                        r
                    }
                    Some(e) if vj.jk().counted() => {
                        let r = self.value_as(e, vj);
                        self.own(r)
                    }
                    Some(e) => self.value_as(e, vj),
                    None => self.zero_rv(vj.jk()),
                };
                self.write_var(*v, val);
            }
            HStmt::MakeClosure(v, g) => {
                // The cell first: the closure may capture its own variable.
                if self.cells.contains_key(v) && !self.declared.contains(v) {
                    self.new_cell(*v);
                }
                let r = self.make_closure(*g);
                self.unpend(r);
                self.write_var(*v, RV { owned: true, ..r });
            }
            HStmt::Block(ss, vars) => {
                for v in vars {
                    if self.cells.contains_key(v) {
                        self.new_cell(*v);
                        self.declared.insert(*v);
                    }
                }
                // A block with scope-bound allocations gets an arena, freed when it ends.
                if self.region_on(ss) {
                    self.enter_region(ss, vars);
                    self.stmts(ss);
                    let i = self.regions.len() - 1;
                    self.leave_region(i);
                    self.regions.pop();
                } else {
                    self.stmts(ss);
                }
            }
            HStmt::If(c, t, e) => {
                let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                let cv = self.truthy(c);
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.seal_block(tb);
                self.b.seal_block(eb);
                self.b.switch_to_block(tb);
                self.stmts(t);
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(eb);
                self.stmts(e);
                self.b.ins().jump(done, &[]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
            }
            HStmt::Loop { label, .. } => self.loop_stmt(s, label.clone()),
            HStmt::Labeled(name, s) if matches!(&**s, HStmt::Loop { label: None, .. }) => self.loop_stmt(s, Some(name.clone())),
            HStmt::Labeled(name, s) if matches!(&**s, HStmt::ForOf { .. }) => self.for_of(s, Some(name.clone())),
            HStmt::Labeled(name, s) if !matches!(&**s, HStmt::Loop { .. }) => {
                let xb = self.b.create_block();
                self.loops.push(LoopT { label: Some(name.clone()), brk: xb, cont: None, switch: false, regions: self.regions.len() });
                self.stmt(s);
                self.loops.pop();
                self.b.ins().jump(xb, &[]);
                self.b.seal_block(xb);
                self.b.switch_to_block(xb);
            }
            HStmt::Labeled(_, s) => self.stmt(s),
            HStmt::Break(label) | HStmt::Continue(label) => {
                let is_break = matches!(s, HStmt::Break(_));
                let target = self.loops.iter().rev().find(|l| match label {
                    Some(n) => l.label.as_ref() == Some(n),
                    None => l.cont.is_some() || (is_break && l.switch),
                });
                let t = target.expect("break target").clone();
                let to = if is_break { t.brk } else { t.cont.expect("continue target") };
                // Leave the regions entered inside the target.
                for i in (t.regions..self.regions.len()).rev() {
                    self.leave_region(i);
                }
                self.b.ins().jump(to, &[]);
                self.dead_block();
            }
            HStmt::Throw(e) if throw_site(e).is_none() => {
                let r = self.value_as(e, JV::Val);
                let r = self.own(r);
                self.b.ins().call(self.h.throw_value, &[r.t.unwrap(), r.v]);
                self.bail();
                self.dead_block();
            }
            HStmt::Switch { label, disc, cases } => self.switch(label, disc, cases),
            HStmt::ForOf { .. } => self.for_of(s, None),
            HStmt::Throw(e) => {
                let site = throw_site(e).unwrap();
                let i = THROW_SITES.with(|t| {
                    let mut t = t.borrow_mut();
                    t.push(site);
                    t.len() - 1
                });
                let i = self.b.ins().iconst(types::I32, i as i64);
                self.b.ins().call(self.h.throw_site, &[i]);
                self.bail();
                self.dead_block();
            }
            HStmt::Return(e) => {
                let r = match e {
                    None => {
                        self.release_vars(None);
                        self.zero_rv(self.rk)
                    }
                    Some(e) if self.rk.counted() => {
                        // Returning an owned local moves its reference out.
                        if let HK::Read(p) = &strip_cast(e).kind
                            && let Place::Var(v) = &**p
                            && self.owned.contains(v)
                            && self.el.vjk(*v) == Some(self.rk)
                            && self.rk == JK::Ref
                        {
                            let r = self.use_var(*v);
                            self.release_vars(Some(*v));
                            self.stats.borrowed += 1;
                            r
                        } else if let HK::Read(p) = &strip_cast(e).kind
                            && let Place::Var(v) = &**p
                            && self.owned.contains(v)
                            && self.el.vjk(*v) == Some(JK::Val)
                            && self.rjv == JV::Val
                        {
                            let r = self.use_var(*v);
                            self.release_vars(Some(*v));
                            self.stats.borrowed += 1;
                            r
                        } else {
                            let r = self.value_as(e, self.rjv);
                            let r = self.own(r);
                            self.release_vars(None);
                            r
                        }
                    }
                    Some(e) => {
                        let r = self.value_as(e, self.rjv);
                        self.release_vars(None);
                        r
                    }
                };
                // Variables are released: free the arenas still open.
                for _ in 0..self.regions.len() {
                    self.b.ins().call(self.h.region_exit, &[]);
                }
                self.ret(r);
                self.dead_block();
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// `while`/`for`/`do` (`label` names it).
    /// A tracing-GC safepoint: collect if the heap has grown enough. Values
    /// live across it are on the stack or in callee-saved registers, which
    /// the collector scans.
    fn gc_poll(&mut self) {
        #[cfg(feature = "gc")]
        let (live, next) = ((&raw const crate::runtime::heap::LIVE_BYTES) as i64, (&raw const crate::runtime::gc::NEXT) as i64);
        #[cfg(not(feature = "gc"))]
        let (live, next) = ((&raw const crate::runtime::heap::LIVE_BYTES) as i64, (&raw const crate::runtime::zct::NEXT) as i64);
        if cfg!(feature = "gc") || crate::runtime::zct::deferred() {
            let live = self.b.ins().iconst(types::I64, live);
            let next = self.b.ins().iconst(types::I64, next);
            let l = self.b.ins().load(types::I64, mf(), live, 0);
            let n = self.b.ins().load(types::I64, mf(), next, 0);
            let due = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, l, n);
            let (call_b, cont) = (self.b.create_block(), self.b.create_block());
            self.b.ins().brif(due, call_b, &[], cont, &[]);
            self.b.set_cold_block(call_b);
            self.b.seal_block(call_b);
            self.b.switch_to_block(call_b);
            self.b.ins().call(self.h.gc_poll, &[]);
            self.b.ins().jump(cont, &[]);
            self.b.seal_block(cont);
            self.b.switch_to_block(cont);
        }
    }

    fn loop_stmt(&mut self, s: &HStmt, label: Option<Name>) {
        let HStmt::Loop { test, body, update, test_after, fresh, .. } = s else { unreachable!() };
        let poll = !fresh.is_empty() || loop_may_allocate(body, test.iter().chain(update.iter()));
        let (tb, bb, ub, xb) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().jump(if *test_after { bb } else { tb }, &[]);
        self.b.switch_to_block(tb);
        if poll {
            self.gc_poll();
        }
        match test {
            Some(t) => {
                let cv = self.truthy(t);
                self.b.ins().brif(cv, bb, &[], xb, &[]);
            }
            None => {
                self.b.ins().jump(bb, &[]);
            }
        }
        self.b.switch_to_block(bb);
        self.loops.push(LoopT { label: label.clone(), brk: xb, cont: Some(ub), switch: false, regions: self.regions.len() });
        self.stmts(body);
        self.loops.pop();
        self.b.ins().jump(ub, &[]);
        self.b.switch_to_block(ub);
        self.b.seal_block(ub);
        // Each iteration's closures keep their own copy of a captured loop variable.
        for v in fresh {
            if let Some(x) = self.cells.get(v).copied() {
                let old = self.b.use_var(x);
                let r = self.load_slot(old, self.lay.cell_value, JV::Val, [0; 3]);
                self.inc_rv(r);
                let call = self.b.ins().call(self.h.new_cell, &[]);
                let c = self.b.inst_results(call)[0];
                self.b.def_var(x, c);
                self.store_slot(c, self.lay.cell_value, JV::Val, r);
                self.dec(old);
            }
        }
        if let Some(u) = update {
            self.discard(u);
        }
        self.b.ins().jump(tb, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(bb);
        self.b.seal_block(xb);
        self.b.switch_to_block(xb);
    }

    /// `switch`: the cases' tests in order (`===`), then the bodies, each
    /// falling through to the next.
    fn switch(&mut self, label: &Option<Name>, disc: &HExpr, cases: &[(Option<HExpr>, Vec<HStmt>)]) {
        let dj = jv(&disc.ty).unwrap();
        let d = self.value_of(disc);
        let var = is_var_read(self.hir, disc);
        let d = if dj.jk().counted() && !d.owned && cases.iter().any(|c| c.0.as_ref().is_some_and(|t| self.el.effects(t, var))) { self.hold(d) } else { d };
        let chosen = self.b.create_block();
        self.b.append_block_param(chosen, types::I32);
        for (k, (t, _)) in cases.iter().enumerate() {
            let Some(t) = t else { continue };
            let tj = jv(&t.ty).unwrap();
            let hit = match (dj, tj) {
                (JV::Num, JV::Num) => {
                    let y = self.expr(t);
                    self.b.ins().fcmp(FloatCC::Equal, d.v, y)
                }
                (JV::Bool, JV::Bool) => {
                    let y = self.expr(t);
                    self.b.ins().icmp(IntCC::Equal, d.v, y)
                }
                _ => {
                    let x = self.coerce(RV { owned: false, ..d }, dj, JV::Val);
                    let y = self.value_as(t, JV::Val);
                    let h = self.strict_eq(x, y);
                    self.release(y);
                    h
                }
            };
            let next = self.b.create_block();
            let kc = self.b.ins().iconst(types::I32, k as i64);
            self.b.ins().brif(hit, chosen, &[kc.into()], next, &[]);
            self.b.seal_block(next);
            self.b.switch_to_block(next);
        }
        let n = cases.len();
        let default = cases.iter().position(|c| c.0.is_none()).unwrap_or(n);
        let dc = self.b.ins().iconst(types::I32, default as i64);
        self.b.ins().jump(chosen, &[dc.into()]);
        self.b.seal_block(chosen);
        self.b.switch_to_block(chosen);
        let i = self.b.block_params(chosen)[0];
        self.release(d);
        let bodies: Vec<Block> = (0..n).map(|_| self.b.create_block()).collect();
        let xb = self.b.create_block();
        let mut sw = cranelift_frontend::Switch::new();
        for (k, blk) in bodies.iter().enumerate() {
            sw.set_entry(k as u128, *blk);
        }
        sw.emit(&mut self.b, i, xb);
        self.loops.push(LoopT { label: label.clone(), brk: xb, cont: None, switch: true, regions: self.regions.len() });
        for (k, (_, body)) in cases.iter().enumerate() {
            self.b.switch_to_block(bodies[k]);
            self.b.seal_block(bodies[k]);
            self.stmts(body);
            self.b.ins().jump(*bodies.get(k + 1).unwrap_or(&xb), &[]);
        }
        self.loops.pop();
        self.b.seal_block(xb);
        self.b.switch_to_block(xb);
    }

    /// `for (x of iter)`: over an array as it is (its length read each
    /// time round), or over an array of another iterable's values.
    fn for_of(&mut self, s: &HStmt, outer: Option<Name>) {
        let HStmt::ForOf { label, elem, iter, body, .. } = s else { unreachable!() };
        let label = label.clone().or(outer);
        let held = self.iters[&(s as *const HStmt as usize)];
        let ij = jv(&iter.ty).unwrap();
        let a = if ij == JV::Arr {
            let r = self.value_as(iter, JV::Arr);
            let r = self.own(r);
            // null or undefined: the interpreter's error.
            let nullish = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, r.v, 2);
            let (t, p) = self.value_words(JV::Arr, r);
            self.if_then(nullish, |s| {
                s.b.ins().call(s.h.iter_array, &[t, p]);
            });
            self.check_status();
            r.v
        } else {
            let r = self.value_as(iter, JV::Val);
            let call = self.b.ins().call(self.h.iter_array, &[r.t.unwrap(), r.v]);
            let a = self.b.inst_results(call)[0];
            self.release(r);
            self.check_status();
            a
        };
        let old = self.b.use_var(held);
        self.b.def_var(held, a);
        self.dec(old);
        let iv = self.b.declare_var(types::F64);
        let zero = self.b.ins().f64const(0.0);
        self.b.def_var(iv, zero);
        let (tb, bb, ub, xb) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().jump(tb, &[]);
        self.b.switch_to_block(tb);
        if self.cells.contains_key(elem) || loop_may_allocate(body, std::iter::empty()) {
            self.gc_poll();
        }
        let a = self.b.use_var(held);
        let len = self.arr_len(a);
        let i = self.b.use_var(iv);
        let more = self.b.ins().fcmp(FloatCC::LessThan, i, len);
        self.b.ins().brif(more, bb, &[], xb, &[]);
        self.b.switch_to_block(bb);
        let ej = var_jv(self.hir, *elem);
        let r = self.arr_get(a, i, ej);
        // A new binding each time round (a new cell, if captured).
        if self.cells.contains_key(elem) {
            self.new_cell(*elem);
        }
        let r = if ej.jk().counted() && !self.borrowed.contains(elem) { self.own(r) } else { self.disown(r) };
        self.write_var(*elem, r);
        self.loops.push(LoopT { label, brk: xb, cont: Some(ub), switch: false, regions: self.regions.len() });
        self.stmts(body);
        self.loops.pop();
        self.b.ins().jump(ub, &[]);
        self.b.switch_to_block(ub);
        self.b.seal_block(ub);
        let i = self.b.use_var(iv);
        let one = self.b.ins().f64const(1.0);
        let i1 = self.b.ins().fadd(i, one);
        self.b.def_var(iv, i1);
        self.b.ins().jump(tb, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(bb);
        self.b.seal_block(xb);
        self.b.switch_to_block(xb);
        let a = self.b.use_var(held);
        self.dec(a);
        let none = self.b.ins().iconst(types::I64, 1);
        self.b.def_var(held, none);
    }

    /// A variable for each `for-of`'s array, released with the variables.
    fn declare_iters(&mut self, ss: &[HStmt]) {
        for s in ss {
            if matches!(s, HStmt::ForOf { .. }) {
                let x = self.b.declare_var(types::I64);
                let none = self.b.ins().iconst(types::I64, 1);
                self.b.def_var(x, none);
                self.iters.insert(s as *const HStmt as usize, x);
            }
            for b in parts(s).1 {
                self.declare_iters(b);
            }
        }
    }

    /// Does block `ss` get an arena?
    fn region_on(&self, ss: &[HStmt]) -> bool {
        arenas_on() && !ss.is_empty() && self.esc.regions.contains(&(ss.as_ptr() as usize))
    }

    /// Enter block `ss`'s region: a new arena.
    fn enter_region(&mut self, ss: &[HStmt], vars: &[VarId]) {
        let call = self.b.ins().call(self.h.region_enter, &[]);
        let arena = self.b.inst_results(call)[0];
        let mut all = vars.to_vec();
        let mut iters = Vec::new();
        block_decls(ss, &mut all, &mut iters);
        let vars = all.into_iter().filter(|v| self.owned.contains(v)).collect();
        self.regions.push(RegionT { key: ss.as_ptr() as usize, arena, vars, iters });
    }

    /// Leave region `i` (on the way out of its block): release its block's
    /// variables (which may point into the arena), then free the arena.
    fn leave_region(&mut self, i: usize) {
        let vars = self.regions[i].vars.clone();
        for v in vars {
            let r = self.use_var(v);
            self.dec_rv(r);
            let z = self.zero_rv(self.el.vjk(v).unwrap());
            self.def_var(v, z);
        }
        for k in self.regions[i].iters.clone() {
            if let Some(x) = self.iters.get(&k).copied() {
                let a = self.b.use_var(x);
                self.dec(a);
                let none = self.b.ins().iconst(types::I64, 1);
                self.b.def_var(x, none);
            }
        }
        self.b.ins().call(self.h.region_exit, &[]);
    }

    /// The arena allocation (or call result) `key` goes in, if the escape
    /// analysis bound it to a scope: a region's, or the caller's result arena.
    fn arena_for(&mut self, key: usize) -> Option<CV> {
        if !arenas_on() {
            return None;
        }
        if self.esc.result.contains(&key) {
            return self.rarena;
        }
        let blk = self.esc.sites.get(&key)?;
        self.regions.iter().rev().find(|r| r.key == *blk).map(|r| r.arena)
    }

    /// Evaluate for effect.
    fn discard(&mut self, e: &HExpr) {
        if jk(&e.ty).unwrap().counted() {
            let r = self.rexpr(e);
            self.release(r);
        } else {
            self.expr(e);
        }
    }

    /// Store a value (owned, if counted) into a variable, releasing the old one.
    #[track_caller]
    fn set_var(&mut self, v: VarId, val: RV) {
        if self.el.vjk(v).unwrap().counted() && !self.borrowed.contains(&v) {
            let old = self.use_var(v);
            self.def_var(v, val);
            self.dec_rv(old);
        } else {
            self.def_var(v, val);
        }
    }

    /// A condition as an i8 (0/1).
    fn truthy(&mut self, e: &HExpr) -> CV {
        match &e.kind {
            HK::Unary(HUnary::Truthy, x) => return self.truthy(x),
            HK::Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) => {
                let (rb, done) = (self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, types::I8);
                let ta = self.truthy(a);
                if *op == LogicalOp::And {
                    self.b.ins().brif(ta, rb, &[], done, &[ta.into()]);
                } else {
                    self.b.ins().brif(ta, done, &[ta.into()], rb, &[]);
                }
                self.b.seal_block(rb);
                self.b.switch_to_block(rb);
                let tb = self.truthy(b);
                self.b.ins().jump(done, &[tb.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                return self.b.block_params(done)[0];
            }
            _ => {}
        }
        match jk(&e.ty).unwrap() {
            k if k.counted() => {
                let r = self.rexpr(e);
                let t = self.truth_rv(r, k);
                self.release(r);
                t
            }
            k => {
                let v = self.expr(e);
                self.truth(v, k)
            }
        }
    }

    fn truth(&mut self, v: CV, k: JK) -> CV {
        match k {
            JK::Bool => v,
            JK::Num => {
                let z = self.b.ins().f64const(0.0);
                self.b.ins().fcmp(FloatCC::OrderedNotEqual, v, z)
            }
            JK::Ref => self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, v, 2),
            JK::Val => unreachable!("use truth_rv"),
        }
    }

    fn truth_rv(&mut self, r: RV, k: JK) -> CV {
        match r.t {
            Some(t) => self.truth_val(t, r.v),
            None => self.truth(r.v, k),
        }
    }

    /// A value's truth: inline for undefined, null, booleans, numbers,
    /// objects and arrays; through the helper for the rest (strings, …).
    fn truth_val(&mut self, t: CV, p: CV) -> CV {
        let l = self.lay;
        let (sb, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I8);
        let low = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, t, l.tag_first_gc);
        let is_obj = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_obj);
        let is_arr = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_arr);
        let is_box = self.b.ins().bor(is_obj, is_arr);
        let simple = self.b.ins().bor(low, is_box);
        self.b.ins().brif(simple, sb, &[], slow, &[]);
        self.b.seal_block(sb);
        self.b.seal_block(slow);
        self.b.switch_to_block(sb);
        let low_byte = self.b.ins().band_imm_u(p, 0xff);
        let bool_t = self.b.ins().icmp_imm_u(IntCC::NotEqual, low_byte, 0);
        let f = self.b.ins().bitcast(types::F64, MemFlagsData::new(), p);
        let z = self.b.ins().f64const(0.0);
        let num_t = self.b.ins().fcmp(FloatCC::OrderedNotEqual, f, z);
        let is_bool = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_bool);
        let is_num = self.b.ins().icmp_imm_u(IntCC::Equal, t, l.tag_num);
        let r = self.b.ins().select(is_bool, bool_t, is_box);
        let r = self.b.ins().select(is_num, num_t, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(slow);
        let r = self.op(OP_TRUTHY, t, p, None);
        let r = self.b.ins().ireduce(types::I8, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// `h_op` on one or two values' words: its raw result.
    fn op(&mut self, op: u32, t: CV, p: CV, b: Option<RV>) -> CV {
        let o = self.b.ins().iconst(types::I32, op as i64);
        let (bt, bp) = match b {
            Some(r) => (r.t.unwrap(), r.v),
            None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
        };
        let c = self.b.ins().call(self.h.op, &[o, t, p, bt, bp]);
        self.b.inst_results(c)[0]
    }

    /// An untyped operator (`h_dyn`) on `a` (and `b`), its result a `k`.
    /// A value operand that isn't a variable's is held: user code
    /// (`toString`) may run.
    fn dyn_op(&mut self, op: u32, a: &HExpr, b: Option<&HExpr>, k: JK) -> RV {
        let x = self.value_as(a, JV::Val);
        let x = if x.owned || is_var_read(self.hir, a) { x } else { self.hold(x) };
        let y = b.map(|b| {
            let y = self.value_as(b, JV::Val);
            if y.owned || is_var_read(self.hir, b) { y } else { self.hold(y) }
        });
        let o = self.b.ins().iconst(types::I32, op as i64);
        let (bt, bp) = match y {
            Some(r) => (r.t.unwrap(), r.v),
            None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
        };
        // Numbers (the common case) inline, as the interpreter does; the
        // rest through the helper.
        let fast: Option<Box<dyn Fn(&mut FunctionBuilder, CV, CV) -> CV>> = match op {
            _ if op == DYN_ADD || op == DYN_NUM || op == DYN_NUMERIC => Some(Box::new(|b, x, y| b.ins().fadd(x, y))),
            _ if op == DYN_NUM + 1 || op == DYN_NUMERIC + 1 => Some(Box::new(|b, x, y| b.ins().fsub(x, y))),
            _ if op == DYN_NUM + 2 || op == DYN_NUMERIC + 2 => Some(Box::new(|b, x, y| b.ins().fmul(x, y))),
            _ if op == DYN_NUM + 3 || op == DYN_NUMERIC + 3 => Some(Box::new(|b, x, y| b.ins().fdiv(x, y))),
            _ if op == DYN_TO_NUM => Some(Box::new(|_, x, _| x)),
            _ if (DYN_CMP..DYN_CMP + 6).contains(&op) => {
                let cc = [FloatCC::LessThan, FloatCC::LessThanOrEqual, FloatCC::GreaterThan, FloatCC::GreaterThanOrEqual, FloatCC::Equal, FloatCC::NotEqual]
                    [(op - DYN_CMP) as usize];
                Some(Box::new(move |b, x, y| b.ins().fcmp(cc, x, y)))
            }
            _ => None,
        };
        let tys: &[types::Type] = match k {
            JK::Val => &[types::I64, types::I64],
            JK::Num => &[types::F64],
            _ => &[types::I8],
        };
        let merge = self.b.create_block();
        for t in tys {
            self.b.append_block_param(merge, *t);
        }
        let slow = self.b.create_block();
        if let Some(f) = &fast {
            let tn = self.lay.tag_num;
            let xn = self.b.ins().icmp_imm_u(IntCC::Equal, x.t.unwrap(), tn);
            let yn = if y.is_some() { self.b.ins().icmp_imm_u(IntCC::Equal, bt, tn) } else { self.b.ins().iconst(types::I8, 1) };
            let both = self.b.ins().band(xn, yn);
            let fb = self.b.create_block();
            self.b.ins().brif(both, fb, &[], slow, &[]);
            self.b.seal_block(fb);
            self.b.switch_to_block(fb);
            let fx = self.b.ins().bitcast(types::F64, MemFlagsData::new(), x.v);
            let fy = self.b.ins().bitcast(types::F64, MemFlagsData::new(), bp);
            let r = f(&mut self.b, fx, fy);
            let args: Vec<cranelift_codegen::ir::BlockArg> = match k {
                JK::Val => {
                    let t = self.b.ins().iconst(types::I64, tn);
                    let p = self.b.ins().bitcast(types::I64, MemFlagsData::new(), r);
                    vec![t.into(), p.into()]
                }
                _ => vec![r.into()],
            };
            self.b.ins().jump(merge, &args);
        } else {
            self.b.ins().jump(slow, &[]);
        }
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let c = self.b.ins().call(self.h.dyn_op, &[o, x.t.unwrap(), x.v, bt, bp]);
        let p = self.b.inst_results(c)[0];
        let args: Vec<cranelift_codegen::ir::BlockArg> = match k {
            JK::Val => {
                let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
                vec![t.into(), p.into()]
            }
            JK::Num => vec![self.b.ins().bitcast(types::F64, MemFlagsData::new(), p).into()],
            _ => vec![self.b.ins().ireduce(types::I8, p).into()],
        };
        self.b.ins().jump(merge, &args);
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
        let ps = self.b.block_params(merge).to_vec();
        let r = match k {
            JK::Val => {
                let r = self.fresh(RV::val(ps[0], ps[1]));
                self.check_status();
                r
            }
            JK::Num => {
                self.check_status();
                RV::new(ps[0])
            }
            _ => RV::new(ps[0]),
        };
        self.release(x);
        if let Some(y) = y {
            self.release(y);
        }
        r
    }

    /// A value result of a helper (its payload `p`, its tag in the context): owned.
    fn helper_val(&mut self, p: CV) -> RV {
        let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
        let r = self.fresh(RV::val(t, p));
        self.check_status();
        r
    }

    fn int32(&mut self, v: CV) -> CV {
        let c = self.b.ins().call(self.h.int32, &[v]);
        self.b.inst_results(c)[0]
    }

    fn num_op(&mut self, op: NumOp, a: CV, b: CV) -> CV {
        let ins = self.b.ins();
        match op {
            NumOp::Add => ins.fadd(a, b),
            NumOp::Sub => ins.fsub(a, b),
            NumOp::Mul => ins.fmul(a, b),
            NumOp::Div => ins.fdiv(a, b),
            NumOp::Mod => {
                // Positive integers (exactly representable): integer
                // remainder; anything else (signs, zero, fractions, NaN):
                // fmod, as the interpreter.
                let ai = self.b.ins().fcvt_to_sint_sat(types::I64, a);
                let bi = self.b.ins().fcvt_to_sint_sat(types::I64, b);
                let af = self.b.ins().fcvt_from_sint(types::F64, ai);
                let bf = self.b.ins().fcvt_from_sint(types::F64, bi);
                let ea = self.b.ins().fcmp(FloatCC::Equal, af, a);
                let eb = self.b.ins().fcmp(FloatCC::Equal, bf, b);
                let pa = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, ai, 0);
                let pb = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, bi, 0);
                let ok = self.b.ins().band(ea, eb);
                let ok = self.b.ins().band(ok, pa);
                let ok = self.b.ins().band(ok, pb);
                let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.append_block_param(done, types::F64);
                self.b.ins().brif(ok, fast, &[], slow, &[]);
                self.b.seal_block(fast);
                self.b.seal_block(slow);
                self.b.switch_to_block(fast);
                let r = self.b.ins().srem(ai, bi);
                let r = self.b.ins().fcvt_from_sint(types::F64, r);
                self.b.ins().jump(done, &[r.into()]);
                self.b.switch_to_block(slow);
                let c = self.b.ins().call(self.h.modf, &[a, b]);
                let r = self.b.inst_results(c)[0];
                self.b.ins().jump(done, &[r.into()]);
                self.b.seal_block(done);
                self.b.switch_to_block(done);
                self.b.block_params(done)[0]
            }
            NumOp::Exp => {
                let h = self.h.pow;
                let c = self.b.ins().call(h, &[a, b]);
                self.b.inst_results(c)[0]
            }
            _ => {
                let (x, y) = (self.int32(a), self.int32(b));
                let r = match op {
                    NumOp::BitAnd => self.b.ins().band(x, y),
                    NumOp::BitOr => self.b.ins().bor(x, y),
                    NumOp::BitXor => self.b.ins().bxor(x, y),
                    NumOp::Shl => self.b.ins().ishl(x, y),
                    NumOp::Shr => self.b.ins().sshr(x, y),
                    _ => {
                        let u = self.b.ins().ushr(x, y);
                        let w = self.b.ins().uextend(types::I64, u);
                        return self.b.ins().fcvt_from_uint(types::F64, w);
                    }
                };
                self.b.ins().fcvt_from_sint(types::F64, r)
            }
        }
    }

    // ── conversions ──

    /// Convert `r` from a `from` to a `to`, as codegen's `coerce`: boxing,
    /// or unboxing with a check (a type error). A count carries over.
    fn coerce(&mut self, r: RV, from: JV, to: JV) -> RV {
        let (fk, tk) = (from.jk(), to.jk());
        if fk == tk {
            return r;
        }
        let l = self.lay;
        let out = match (fk, tk) {
            (_, JK::Val) => {
                let (t, p) = self.value_words(from, r);
                RV { v: p, t: Some(t), owned: r.owned }
            }
            (JK::Val, JK::Num) => {
                let t = r.t.unwrap();
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, t, l.tag_num);
                self.throw_if(bad, ERR_UNBOX_NUM, t, r.v);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), r.v))
            }
            (JK::Val, JK::Bool) => {
                let t = r.t.unwrap();
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, t, l.tag_bool);
                self.throw_if(bad, ERR_UNBOX_BOOL, t, r.v);
                RV::new(self.b.ins().ireduce(types::I8, r.v))
            }
            (JK::Val, JK::Ref) => RV { v: self.unbox_ref(r.t.unwrap(), r.v, to, ERR_UNBOX_REF), t: None, owned: r.owned },
            _ => unreachable!("not coercible: {from:?} to {to:?}"),
        };
        self.repend(r, out);
        out
    }

    /// `e`, whatever its kind (a reference or value as `rexpr` gives it).
    fn value_of(&mut self, e: &HExpr) -> RV {
        if jk(&e.ty).unwrap().counted() { self.rexpr(e) } else { RV::new(self.expr(e)) }
    }

    /// `e`, converted to a `to`.
    fn value_as(&mut self, e: &HExpr, to: JV) -> RV {
        let r = self.value_of(e);
        self.coerce(r, jv(&e.ty).unwrap(), to)
    }

    // ── fields ──

    /// The object of a field access, checked. Held across `rest` (code that
    /// runs before the access) it must be owned unless nothing in `rest`
    /// could free it.
    fn field_base(&mut self, obj: &HExpr, rest: Option<&HExpr>, code: u32) -> RV {
        let r = self.value_as(obj, JV::Obj);
        let r = self.hold_across(r, obj, rest.as_slice());
        self.check_obj(r.v, code);
        r
    }

    /// A value's tag and payload words (both i64).
    fn value_words(&mut self, vj: JV, r: RV) -> (CV, CV) {
        let v = r.v;
        match vj {
            JV::Num => (self.b.ins().iconst(types::I64, self.lay.tag_num), self.b.ins().bitcast(types::I64, MemFlagsData::new(), v)),
            JV::Bool => (self.b.ins().iconst(types::I64, self.lay.tag_bool), self.b.ins().uextend(types::I64, v)),
            JV::Val => (r.t.unwrap(), v),
            _ => {
                let heap = if vj == JV::Arr { self.lay.tag_arr } else { self.lay.tag_obj };
                let obj = self.b.ins().iconst(types::I64, heap);
                let null = self.b.ins().iconst(types::I64, self.lay.tag_null);
                let undef = self.b.ins().iconst(types::I64, self.lay.tag_undef);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, v, 0);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, v, 1);
                let t = self.b.ins().select(is_undef, undef, obj);
                (self.b.ins().select(is_null, null, t), v)
            }
        }
    }

    /// Check the words of a slot's value (at `at + off`) and convert to a
    /// `want` (borrowed). `codes` are the errors for a number, a boolean
    /// and a reference.
    fn load_slot(&mut self, at: CV, off: i32, want: JV, codes: [u32; 3]) -> RV {
        let tag = self.b.ins().load(types::I8, mf(), at, off);
        let tag = self.b.ins().uextend(types::I64, tag);
        let payload = self.b.ins().load(types::I64, mf(), at, off + 8);
        match want {
            JV::Val => RV::val(tag, payload),
            JV::Num => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_num);
                self.throw_if(bad, codes[0], tag, payload);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), payload))
            }
            JV::Bool => {
                let bad = self.b.ins().icmp_imm_u(IntCC::NotEqual, tag, self.lay.tag_bool);
                self.throw_if(bad, codes[1], tag, payload);
                RV::new(self.b.ins().load(types::I8, mf(), at, off + 8))
            }
            _ => RV::new(self.unbox_ref(tag, payload, want, codes[2])),
        }
    }

    /// A value's words as a reference to a `to` (an instance or array; 0
    /// for null, 1 for undefined), throwing error `code` for anything else.
    fn unbox_ref(&mut self, tag: CV, payload: CV, to: JV, code: u32) -> CV {
        let heap = match to {
            JV::Arr => self.lay.tag_arr,
            JV::Obj => self.lay.tag_obj,
            _ => -1,
        };
        let is_heap = self.b.ins().icmp_imm_u(IntCC::Equal, tag, heap);
        let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_null);
        let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_undef);
        let any = self.b.ins().bor(is_heap, is_null);
        let any = self.b.ins().bor(any, is_undef);
        let bad = self.b.ins().bxor_imm_u(any, 1);
        self.throw_if(bad, code, tag, payload);
        let zero = self.b.ins().iconst(types::I64, 0);
        let one = self.b.ins().iconst(types::I64, 1);
        let nullish = self.b.ins().select(is_null, zero, one);
        self.b.ins().select(is_heap, payload, nullish)
    }

    /// Read field `slot` of the object `p` as `want` (borrowed).
    fn load_field(&mut self, p: CV, slot: u32, want: JV) -> RV {
        let off = Object::field_offset(slot as usize) as i32;
        self.load_slot(p, off, want, [ERR_FIELD_NUM, ERR_FIELD_BOOL, ERR_FIELD_REF])
    }

    /// Store `r` (a `vj`, owned if counted) into the slot at `at + off`,
    /// then release the old value.
    #[track_caller]
    fn store_slot(&mut self, at: CV, off: i32, vj: JV, r: RV) {
        let old_tag = self.b.ins().load(types::I8, mf(), at, off);
        let old_tag = self.b.ins().uextend(types::I64, old_tag);
        let old = self.b.ins().load(types::I64, mf(), at, off + 8);
        let (tag, payload) = self.value_words(vj, r);
        self.b.ins().istore8(mf(), tag, at, off);
        self.b.ins().store(mf(), payload, at, off + 8);
        self.release_old(old_tag, old);
    }

    /// Store `r` (a `vj`, owned if counted) into field `slot` of `p`.
    #[track_caller]
    fn store_field(&mut self, p: CV, slot: u32, vj: JV, r: RV) {
        let off = Object::field_offset(slot as usize) as i32;
        self.store_slot(p, off, vj, r);
    }

    /// Release a value overwritten in a slot (its tag, widened, and payload).
    #[track_caller]
    fn release_old(&mut self, old_tag: CV, old: CV) {
        self.val_count(old_tag, old, false);
    }

    /// Take (`inc`) or release a reference to a value given as words:
    /// nothing unless it may own a box; inline for an object or array,
    /// through a helper for other boxed kinds.
    #[track_caller]
    fn val_count(&mut self, t: CV, p: CV, inc: bool) {
        if inc {
            self.stats.incs += 1;
        } else {
            self.stats.decs += 1;
        }
        if cfg!(feature = "gc") {
            return;
        }
        let loc = std::panic::Location::caller();
        let boxed = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, t, self.lay.tag_first_gc);
        let (tag_obj, tag_arr) = (self.lay.tag_obj, self.lay.tag_arr);
        self.if_then(boxed, |s| {
            s.count_site(loc, inc);
            let is_obj = s.b.ins().icmp_imm_u(IntCC::Equal, t, tag_obj);
            let is_arr = s.b.ins().icmp_imm_u(IntCC::Equal, t, tag_arr);
            let is_box = s.b.ins().bor(is_obj, is_arr);
            let (ob, other, done) = (s.b.create_block(), s.b.create_block(), s.b.create_block());
            s.b.ins().brif(is_box, ob, &[], other, &[]);
            s.b.seal_block(ob);
            s.b.seal_block(other);
            s.b.switch_to_block(ob);
            if inc {
                s.inc_box(p);
            } else {
                s.dec_box(p);
            }
            s.b.ins().jump(done, &[]);
            s.b.switch_to_block(other);
            let h = if inc { s.h.inc_val } else { s.h.drop_val };
            s.b.ins().call(h, &[t, p]);
            s.b.ins().jump(done, &[]);
            s.b.seal_block(done);
            s.b.switch_to_block(done);
        });
    }

    // ── arrays ──

    /// The array of an element access; held (owned) if `rest` (evaluated
    /// before the access) could free it.
    fn arr_base(&mut self, obj: &HExpr, rest: &[&HExpr]) -> RV {
        let r = self.value_as(obj, JV::Arr);
        self.hold_across(r, obj, rest)
    }

    /// `r` (the value of `src`), owned if running `later` could free or
    /// change it before it is used. A variable read can only be changed by
    /// an assignment; anything else by any code that runs.
    fn hold_across(&mut self, r: RV, src: &HExpr, later: &[&HExpr]) -> RV {
        let var = is_var_read(self.hir, src);
        if !r.owned && later.iter().any(|x| self.el.effects(x, var)) { self.hold(r) } else { r }
    }

    /// `i` as an index into the array at `a` (a real array, `al` its
    /// layout), and whether it is an integer within bounds.
    fn arr_index(&mut self, a: CV, i: CV, al: ArrLayout) -> (CV, CV) {
        let ii = self.b.ins().fcvt_to_sint_sat(types::I64, i);
        let back = self.b.ins().fcvt_from_sint(types::F64, ii);
        let exact = self.b.ins().fcmp(FloatCC::Equal, back, i);
        let len = self.b.ins().load(types::I64, mf(), a, al.len);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, ii, len);
        (ii, self.b.ins().band(exact, inb))
    }

    /// Branch to `fast` if `a` is an array whose store variant is `variant`
    /// (any, if None) and `i` an index within it; else to `slow`. Returns
    /// the integer index (valid in `fast`).
    fn arr_guard(&mut self, a: CV, i: CV, al: ArrLayout, variant: Option<i64>, fast: Block, slow: Block) -> CV {
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, a, 2);
        let chk = self.b.create_block();
        self.b.ins().brif(real, chk, &[], slow, &[]);
        self.b.seal_block(chk);
        self.b.switch_to_block(chk);
        let (ii, ok) = self.arr_index(a, i, al);
        let ok = match variant {
            Some(v) => {
                let var = self.b.ins().load(types::I8, mf(), a, al.variant);
                let is = self.b.ins().icmp_imm_u(IntCC::Equal, var, v);
                self.b.ins().band(ok, is)
            }
            None => ok,
        };
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        ii
    }

    /// Read `a[i]` as `want` (borrowed): inline for an in-bounds integer
    /// index into the expected storage, else through the helper.
    fn arr_get(&mut self, a: CV, i: CV, want: JV) -> RV {
        let Some(al) = self.lay.arr else { return self.arr_get_slow(a, i, want) };
        if want == JV::Val {
            return self.arr_get_val(a, i, al);
        }
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        let ty = cl_ty(want.jk());
        self.b.append_block_param(done, ty);
        let ii = self.arr_guard(a, i, al, Some(if want == JV::Num { 0 } else { 1 }), fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        if want == JV::Num {
            let off = self.b.ins().ishl_imm_u(ii, 3);
            let at = self.b.ins().iadd(data, off);
            let v = self.b.ins().load(types::F64, mf(), at, 0);
            self.b.ins().jump(done, &[v.into()]);
        } else {
            let off = self.b.ins().ishl_imm_u(ii, 4);
            let at = self.b.ins().iadd(data, off);
            let tag = self.b.ins().load(types::I8, mf(), at, 0);
            let tag = self.b.ins().uextend(types::I64, tag);
            let payload = self.b.ins().load(types::I64, mf(), at, 8);
            let (v, good) = if want == JV::Bool {
                (self.b.ins().ireduce(types::I8, payload), self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_bool))
            } else {
                let heap = match want {
                    JV::Arr => self.lay.tag_arr,
                    JV::Obj => self.lay.tag_obj,
                    _ => -1,
                };
                let is_heap = self.b.ins().icmp_imm_u(IntCC::Equal, tag, heap);
                let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_null);
                let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, tag, self.lay.tag_undef);
                let zero = self.b.ins().iconst(types::I64, 0);
                let one = self.b.ins().iconst(types::I64, 1);
                let nullish = self.b.ins().select(is_null, zero, one);
                let v = self.b.ins().select(is_heap, payload, nullish);
                let any = self.b.ins().bor(is_heap, is_null);
                (v, self.b.ins().bor(any, is_undef))
            };
            self.b.ins().brif(good, done, &[v.into()], slow, &[]);
        }
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let v = self.arr_get_slow(a, i, want);
        self.b.ins().jump(done, &[v.v.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        RV::new(self.b.block_params(done)[0])
    }

    /// Read `a[i]` as any value: inline from either storage.
    fn arr_get_val(&mut self, a: CV, i: CV, al: ArrLayout) -> RV {
        let (fast, nums, vals, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I64);
        self.b.append_block_param(done, types::I64);
        let ii = self.arr_guard(a, i, al, None, fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        let var = self.b.ins().load(types::I8, mf(), a, al.variant);
        self.b.ins().brif(var, vals, &[], nums, &[]);
        self.b.seal_block(nums);
        self.b.seal_block(vals);
        self.b.switch_to_block(nums);
        let off = self.b.ins().ishl_imm_u(ii, 3);
        let at = self.b.ins().iadd(data, off);
        let bits = self.b.ins().load(types::I64, mf(), at, 0);
        let tn = self.b.ins().iconst(types::I64, self.lay.tag_num);
        self.b.ins().jump(done, &[tn.into(), bits.into()]);
        self.b.switch_to_block(vals);
        let off = self.b.ins().ishl_imm_u(ii, 4);
        let at = self.b.ins().iadd(data, off);
        let tag = self.b.ins().load(types::I8, mf(), at, 0);
        let tag = self.b.ins().uextend(types::I64, tag);
        let payload = self.b.ins().load(types::I64, mf(), at, 8);
        self.b.ins().jump(done, &[tag.into(), payload.into()]);
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let r = self.arr_get_slow(a, i, JV::Val);
        self.b.ins().jump(done, &[r.t.unwrap().into(), r.v.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let p = self.b.block_params(done);
        RV::val(p[0], p[1])
    }

    /// `a[i] = r` (`r` a `vj`, owned if counted): inline for an in-bounds
    /// integer index (numbers into number storage, anything into value
    /// storage), else through the helper.
    #[track_caller]
    fn arr_set(&mut self, a: CV, i: CV, vj: JV, r: RV) {
        let Some(al) = self.lay.arr else { return self.arr_set_slow(a, i, vj, r) };
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        // Non-numbers only go inline into value storage.
        let ii = self.arr_guard(a, i, al, if vj == JV::Num { None } else { Some(1) }, fast, slow);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        let data = self.b.ins().load(types::I64, mf(), a, al.data);
        let vals = self.b.create_block();
        if vj == JV::Num {
            let var = self.b.ins().load(types::I8, mf(), a, al.variant);
            let nums = self.b.create_block();
            self.b.ins().brif(var, vals, &[], nums, &[]);
            self.b.seal_block(nums);
            self.b.switch_to_block(nums);
            let off = self.b.ins().ishl_imm_u(ii, 3);
            let at = self.b.ins().iadd(data, off);
            self.b.ins().store(mf(), r.v, at, 0);
            self.b.ins().jump(done, &[]);
        } else {
            self.b.ins().jump(vals, &[]);
        }
        self.b.seal_block(vals);
        self.b.switch_to_block(vals);
        let off = self.b.ins().ishl_imm_u(ii, 4);
        let at = self.b.ins().iadd(data, off);
        self.store_slot(at, 0, vj, r);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        self.arr_set_slow(a, i, vj, r);
        self.b.ins().jump(done, &[]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
    }

    /// `a.length`.
    fn arr_len(&mut self, a: CV) -> CV {
        let Some(al) = self.lay.arr else { return self.arr_len_slow(a) };
        let (fast, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::F64);
        let real = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, a, 2);
        self.b.ins().brif(real, fast, &[], slow, &[]);
        self.b.seal_block(fast);
        self.b.seal_block(slow);
        self.b.switch_to_block(fast);
        let n = self.b.ins().load(types::I64, mf(), a, al.len);
        let n = self.b.ins().fcvt_from_uint(types::F64, n);
        self.b.ins().jump(done, &[n.into()]);
        self.b.switch_to_block(slow);
        let n = self.arr_len_slow(a);
        self.b.ins().jump(done, &[n.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    fn arr_len_slow(&mut self, a: CV) -> CV {
        let call = self.b.ins().call(self.h.arr_len, &[a]);
        let n = self.b.inst_results(call)[0];
        self.check_status();
        n
    }

    /// Read `a[i]` as `want` (borrowed), through the helper.
    fn arr_get_slow(&mut self, a: CV, i: CV, want: JV) -> RV {
        let w = match want {
            JV::Num => W_NUM,
            JV::Bool => W_BOOL,
            JV::Obj => W_OBJ,
            JV::Arr => W_ARR,
            JV::Nil => W_NIL,
            JV::Val => W_VAL,
        };
        let w = self.b.ins().iconst(types::I32, w as i64);
        let call = self.b.ins().call(self.h.arr_get, &[a, i, w]);
        let r = self.b.inst_results(call)[0];
        let t = (want == JV::Val).then(|| self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG));
        self.check_status();
        match want {
            JV::Num => RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), r)),
            JV::Bool => RV::new(self.b.ins().ireduce(types::I8, r)),
            JV::Val => RV::val(t.unwrap(), r),
            _ => RV::new(r),
        }
    }

    /// `a[i] = r`, through the helper.
    fn arr_set_slow(&mut self, a: CV, i: CV, vj: JV, r: RV) {
        let (tag, payload) = self.value_words(vj, r);
        let num = self.b.ins().iconst(types::I32, (vj == JV::Num) as i64);
        self.b.ins().call(self.h.arr_set, &[a, i, tag, payload, num]);
        self.check_status();
    }

    /// An owned value of `x` (a reference's count taken), as words for a helper.
    fn owned_words(&mut self, x: &HExpr) -> (CV, CV) {
        let vj = jv(&x.ty).unwrap();
        let r = self.value_of(x);
        let r = if vj.jk().counted() { self.own(r) } else { r };
        self.value_words(vj, r)
    }

    // ── expressions ──

    /// A number or boolean expression.
    fn expr(&mut self, e: &HExpr) -> CV {
        let k = jk(&e.ty).expect("checked by Elig");
        let want = jv(&e.ty).unwrap();
        if k.counted() {
            unreachable!("counted expressions go through rexpr");
        }
        match &e.kind {
            HK::Num(x) => self.b.ins().f64const(*x),
            HK::Bool(x) => self.b.ins().iconst(types::I8, *x as i64),
            HK::Read(p) => self.read(p, &e.ty).v,
            HK::Binary(HBin::Num(op), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                self.num_op(*op, x, y)
            }
            HK::Binary(HBin::NumCmp(op), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                let cc = match op {
                    CmpOp::Lt => FloatCC::LessThan,
                    CmpOp::Le => FloatCC::LessThanOrEqual,
                    CmpOp::Gt => FloatCC::GreaterThan,
                    CmpOp::Ge => FloatCC::GreaterThanOrEqual,
                    CmpOp::Eq => FloatCC::Equal,
                    CmpOp::Ne => FloatCC::NotEqual,
                };
                self.b.ins().fcmp(cc, x, y)
            }
            HK::Binary(HBin::BoolEq(eq), a, b) => {
                let (x, y) = (self.expr(a), self.expr(b));
                self.b.ins().icmp(if *eq { IntCC::Equal } else { IntCC::NotEqual }, x, y)
            }
            HK::Binary(op @ (HBin::StrictEq(eq) | HBin::LooseEq(eq)), a, b) => {
                let loose = matches!(op, HBin::LooseEq(_));
                let (ka, kb) = (jk(&a.ty).unwrap(), jk(&b.ty).unwrap());
                if ka != kb || ka == JK::Val {
                    return self.val_eq(a, b, loose, *eq);
                }
                match ka {
                    JK::Num => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().fcmp(if *eq { FloatCC::Equal } else { FloatCC::NotEqual }, x, y)
                    }
                    JK::Bool => {
                        let (x, y) = (self.expr(a), self.expr(b));
                        self.b.ins().icmp(if *eq { IntCC::Equal } else { IntCC::NotEqual }, x, y)
                    }
                    _ => {
                        // Only identity is compared: a borrowed left side
                        // must stay alive (and unchanged) while the right runs.
                        let x = self.rexpr(a);
                        let x = self.hold_across(x, a, &[b]);
                        let y = self.rexpr(b);
                        let same = self.b.ins().icmp(IntCC::Equal, x.v, y.v);
                        let same = if loose {
                            // null == undefined
                            let xn = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, x.v, 2);
                            let yn = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, y.v, 2);
                            let both = self.b.ins().band(xn, yn);
                            self.b.ins().bor(same, both)
                        } else {
                            same
                        };
                        self.release(x);
                        self.release(y);
                        if *eq { same } else { self.b.ins().bxor_imm_u(same, 1) }
                    }
                }
            }
            HK::Binary(HBin::StrCmp(op), a, b) => {
                let x = self.value_as(a, JV::Val);
                let x = self.hold_across(x, a, &[b]);
                let y = self.value_as(b, JV::Val);
                let r = self.op(OP_STR_CMP + *op as u32, x.t.unwrap(), x.v, Some(y));
                self.release(x);
                self.release(y);
                self.b.ins().ireduce(types::I8, r)
            }
            HK::Unary(HUnary::NegNum, x) => {
                let v = self.expr(x);
                self.b.ins().fneg(v)
            }
            HK::Unary(HUnary::BitNotNum, x) => {
                let v = self.expr(x);
                let i = self.int32(v);
                let n = self.b.ins().bnot(i);
                self.b.ins().fcvt_from_sint(types::F64, n)
            }
            HK::Unary(HUnary::NotBool, x) => {
                let v = self.truthy(x);
                self.b.ins().bxor_imm_u(v, 1)
            }
            HK::Unary(HUnary::Truthy, x) => self.truthy(x),
            HK::Binary(HBin::DynNum(op), a, b) => self.dyn_op(DYN_NUM + *op as u32, a, Some(b), JK::Num).v,
            HK::Binary(HBin::DynCmp(op), a, b) => self.dyn_op(DYN_CMP + *op as u32, a, Some(b), JK::Bool).v,
            HK::Unary(HUnary::ToNum, x) => self.dyn_op(DYN_TO_NUM, x, None, JK::Num).v,
            HK::Compound(p, op @ HBin::DynNum(_), x) => {
                let a = compound_assign(self.hir, p, *op, x, &e.ty).unwrap();
                self.expr(&a)
            }
            HK::Logical(op, a, b) => self.logical(*op, a, b, want).v,
            HK::Cond(c, a, b) => self.cond_expr(c, a, b, want).v,
            HK::Assign(p, x) => self.assign(p, x, want).v,
            HK::Compound(p, HBin::Num(op), x) => {
                let h = self.num_place(p, Some(x));
                let cur = self.num_load(&h);
                let y = self.expr(x);
                let r = self.num_op(*op, cur, y);
                self.num_store(h, r);
                r
            }
            HK::Update { place, inc, prefix } => {
                let h = self.num_place(place, None);
                let cur = self.num_load(&h);
                let one = self.b.ins().f64const(1.0);
                let r = if *inc { self.b.ins().fadd(cur, one) } else { self.b.ins().fsub(cur, one) };
                self.num_store(h, r);
                if *prefix { r } else { cur }
            }
            HK::Cast(x) => self.value_as(x, want).v,
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.value_as(xs.last().unwrap(), want).v
            }
            HK::Call(c) => self.call(e, c, want).v,
            _ => unreachable!("checked by Elig"),
        }
    }

    /// The place of a numeric `+=` or `++`, its object evaluated (and held
    /// across `rest`, the right-hand side, if that could free it).
    fn num_place(&mut self, p: &Place, rest: Option<&HExpr>) -> NumPlace {
        match p {
            Place::Var(v) => NumPlace::Var(*v),
            Place::Field { obj, slot, .. } => NumPlace::Field(self.field_base(obj, rest, ERR_READ), *slot),
            Place::Index { obj, index, .. } => {
                let mut later = vec![index];
                later.extend(rest);
                let a = self.arr_base(obj, &later);
                let i = self.expr(index);
                NumPlace::Index(a, i)
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    fn num_load(&mut self, h: &NumPlace) -> CV {
        match *h {
            NumPlace::Var(v) => self.read_var(v).v,
            NumPlace::Field(o, slot) => self.load_field(o.v, slot, JV::Num).v,
            NumPlace::Index(a, i) => self.arr_get(a.v, i, JV::Num).v,
        }
    }

    /// Store into the place, then release its object.
    fn num_store(&mut self, h: NumPlace, r: CV) {
        match h {
            NumPlace::Var(v) => self.write_var(v, RV::new(r)),
            NumPlace::Field(o, slot) => {
                self.store_field(o.v, slot, JV::Num, RV::new(r));
                self.release(o);
            }
            NumPlace::Index(a, i) => {
                self.arr_set(a.v, i, JV::Num, RV::new(r));
                self.release(a);
            }
        }
    }

    /// `a === b` / `a == b` (or `!==`, `!=`) on values.
    fn val_eq(&mut self, a: &HExpr, b: &HExpr, loose: bool, eq: bool) -> CV {
        let x = self.value_as(a, JV::Val);
        let x = self.hold_across(x, a, &[b]);
        let y = self.value_as(b, JV::Val);
        let r = if loose {
            let r = self.op(OP_LOOSE_EQ, x.t.unwrap(), x.v, Some(y));
            self.b.ins().ireduce(types::I8, r)
        } else {
            self.strict_eq(x, y)
        };
        self.release(x);
        self.release(y);
        if eq { r } else { self.b.ins().bxor_imm_u(r, 1) }
    }

    /// `===` on two values' words: inline unless both are strings (or
    /// another kind compared by contents).
    fn strict_eq(&mut self, x: RV, y: RV) -> CV {
        let l = self.lay;
        let (xt, yt) = (x.t.unwrap(), y.t.unwrap());
        let (same, simple_b, slow, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, types::I8);
        let same_tag = self.b.ins().icmp(IntCC::Equal, xt, yt);
        let f = self.b.ins().iconst(types::I8, 0);
        self.b.ins().brif(same_tag, same, &[], done, &[f.into()]);
        self.b.seal_block(same);
        self.b.switch_to_block(same);
        let is_num = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_num);
        let is_bool = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_bool);
        let is_obj = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_obj);
        let is_arr = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_arr);
        let is_ptr = self.b.ins().bor(is_obj, is_arr);
        let is_null = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_null);
        let is_undef = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_undef);
        let nullish = self.b.ins().bor(is_null, is_undef);
        let s1 = self.b.ins().bor(is_num, is_bool);
        let s2 = self.b.ins().bor(is_ptr, nullish);
        let simple = self.b.ins().bor(s1, s2);
        self.b.ins().brif(simple, simple_b, &[], slow, &[]);
        self.b.seal_block(simple_b);
        self.b.seal_block(slow);
        self.b.switch_to_block(simple_b);
        let xf = self.b.ins().bitcast(types::F64, MemFlagsData::new(), x.v);
        let yf = self.b.ins().bitcast(types::F64, MemFlagsData::new(), y.v);
        let num_eq = self.b.ins().fcmp(FloatCC::Equal, xf, yf);
        let xb = self.b.ins().band_imm_u(x.v, 0xff);
        let yb = self.b.ins().band_imm_u(y.v, 0xff);
        let bool_eq = self.b.ins().icmp(IntCC::Equal, xb, yb);
        let ptr_eq = self.b.ins().icmp(IntCC::Equal, x.v, y.v);
        let one = self.b.ins().iconst(types::I8, 1);
        let r = self.b.ins().select(is_ptr, ptr_eq, one);
        let r = self.b.ins().select(is_bool, bool_eq, r);
        let r = self.b.ins().select(is_num, num_eq, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(slow);
        // Same box: equal. Strings: different lengths differ, and one byte
        // compares inline (one-character strings, mostly shared anyway).
        let same_box = self.b.ins().icmp(IntCC::Equal, x.v, y.v);
        let t1 = self.b.ins().iconst(types::I8, 1);
        let other = self.b.create_block();
        self.b.ins().brif(same_box, done, &[t1.into()], other, &[]);
        self.b.seal_block(other);
        self.b.switch_to_block(other);
        let helper = self.b.create_block();
        if let Some((ptr_off, len_off)) = l.str_bytes {
            let is_str = self.b.ins().icmp_imm_u(IntCC::Equal, xt, l.tag_str);
            let str_b = self.b.create_block();
            self.b.ins().brif(is_str, str_b, &[], helper, &[]);
            self.b.seal_block(str_b);
            self.b.switch_to_block(str_b);
            let xl = self.b.ins().load(types::I64, mf(), x.v, len_off);
            let yl = self.b.ins().load(types::I64, mf(), y.v, len_off);
            let same_len = self.b.ins().icmp(IntCC::Equal, xl, yl);
            let f0 = self.b.ins().iconst(types::I8, 0);
            let len_b = self.b.create_block();
            self.b.ins().brif(same_len, len_b, &[], done, &[f0.into()]);
            self.b.seal_block(len_b);
            self.b.switch_to_block(len_b);
            let one = self.b.ins().icmp_imm_u(IntCC::Equal, xl, 1);
            let byte_b = self.b.create_block();
            self.b.ins().brif(one, byte_b, &[], helper, &[]);
            self.b.seal_block(byte_b);
            self.b.switch_to_block(byte_b);
            let xp = self.b.ins().load(types::I64, mf(), x.v, ptr_off);
            let yp = self.b.ins().load(types::I64, mf(), y.v, ptr_off);
            let xc = self.b.ins().load(types::I8, mf(), xp, 0);
            let yc = self.b.ins().load(types::I8, mf(), yp, 0);
            let r = self.b.ins().icmp(IntCC::Equal, xc, yc);
            self.b.ins().jump(done, &[r.into()]);
        } else {
            self.b.ins().jump(helper, &[]);
        }
        self.b.seal_block(helper);
        self.b.switch_to_block(helper);
        let r = self.op(OP_STRICT_EQ, xt, x.v, Some(y));
        let r = self.b.ins().ireduce(types::I8, r);
        self.b.ins().jump(done, &[r.into()]);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// `a && b` / `a || b`, keeping the value (not just its truth), as a `to`.
    fn logical(&mut self, op: LogicalOp, a: &HExpr, b: &HExpr, to: JV) -> RV {
        let ja = jv(&a.ty).unwrap();
        let counted = to.jk().counted();
        let (keep, rb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        for ty in cl_tys(to.jk()) {
            self.b.append_block_param(done, ty);
        }
        let x = self.value_of(a);
        // Owned (not pending) while its truth decides.
        let x = if ja.jk().counted() { self.own(x) } else { x };
        let t = self.truth_rv(x, ja.jk());
        if op == LogicalOp::And {
            self.b.ins().brif(t, rb, &[], keep, &[]);
        } else {
            self.b.ins().brif(t, keep, &[], rb, &[]);
        }
        self.b.seal_block(keep);
        self.b.seal_block(rb);
        self.b.switch_to_block(keep);
        // The left side is the result.
        let c = if ja.jk().counted() {
            let x = self.fresh(x);
            let c = self.coerce(x, ja, to);
            if c.owned {
                self.unpend(c);
            }
            c
        } else {
            self.coerce(x, ja, to)
        };
        let w: Vec<_> = c.words().into_iter().map(Into::into).collect();
        self.b.ins().jump(done, &w);
        self.b.switch_to_block(rb);
        if ja.jk().counted() {
            self.dec_rv(x);
        }
        let y = self.value_as(b, to);
        let y = if counted { self.own(y) } else { y };
        let w: Vec<_> = y.words().into_iter().map(Into::into).collect();
        self.b.ins().jump(done, &w);
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let r = RV::of_words(&self.b.block_params(done).to_vec());
        if counted { self.fresh(r) } else { r }
    }

    /// `c ? a : b`, as a `to`.
    fn cond_expr(&mut self, c: &HExpr, a: &HExpr, b: &HExpr, to: JV) -> RV {
        let counted = to.jk().counted();
        let (tb, eb, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        for ty in cl_tys(to.jk()) {
            self.b.append_block_param(done, ty);
        }
        let cv = self.truthy(c);
        self.b.ins().brif(cv, tb, &[], eb, &[]);
        self.b.seal_block(tb);
        self.b.seal_block(eb);
        for (blk, x) in [(tb, a), (eb, b)] {
            self.b.switch_to_block(blk);
            let r = self.value_as(x, to);
            let r = if counted { self.own(r) } else { r };
            let w: Vec<_> = r.words().into_iter().map(Into::into).collect();
            self.b.ins().jump(done, &w);
        }
        self.b.seal_block(done);
        self.b.switch_to_block(done);
        let r = RV::of_words(&self.b.block_params(done).to_vec());
        if counted { self.fresh(r) } else { r }
    }

    /// A reference or value expression, as `jv(&e.ty)` holds it.
    fn rexpr(&mut self, e: &HExpr) -> RV {
        let want = jv(&e.ty).unwrap();
        let borrowed = RV::new;
        match &e.kind {
            // As a reference (0 and 1), or as a value when the expression
            // is typed as one (an omitted argument the checker filled in).
            HK::Null | HK::Undefined if want.jk() == JK::Val => {
                let tag = if matches!(e.kind, HK::Null) { self.lay.tag_null } else { self.lay.tag_undef };
                let t = self.b.ins().iconst(types::I64, tag);
                let p = self.b.ins().iconst(types::I64, 0);
                RV::val(t, p)
            }
            HK::Null => borrowed(self.b.ins().iconst(types::I64, 0)),
            HK::Undefined => borrowed(self.b.ins().iconst(types::I64, 1)),
            HK::Str(s) => {
                let [t, p] = constant(Value::str(&**s));
                let t = self.b.ins().iconst(types::I64, t as i64);
                let p = self.b.ins().iconst(types::I64, p as i64);
                RV::val(t, p)
            }
            HK::Read(p) => self.read(p, &e.ty),
            HK::Closure(g) => {
                let r = self.make_closure(*g);
                self.coerce(r, JV::Val, want)
            }
            HK::Assign(p, x) => self.assign(p, x, want),
            HK::Compound(p, op, x) => {
                let a = compound_assign(self.hir, p, *op, x, &e.ty).unwrap();
                self.rexpr(&a)
            }
            HK::Binary(HBin::DynAdd, a, b) => {
                let r = self.dyn_op(DYN_ADD, a, Some(b), JK::Val);
                self.coerce(r, JV::Val, want)
            }
            HK::Binary(HBin::DynNumeric(op), a, b) => {
                let r = self.dyn_op(DYN_NUMERIC + *op as u32, a, Some(b), JK::Val);
                self.coerce(r, JV::Val, want)
            }
            HK::Unary(op @ (HUnary::NegDyn | HUnary::BitNotDyn), x) => {
                let code = if matches!(op, HUnary::NegDyn) { DYN_NEG } else { DYN_BITNOT };
                let r = self.dyn_op(code, x, None, JK::Val);
                self.coerce(r, JV::Val, want)
            }
            HK::Cast(x) => self.value_as(x, want),
            HK::Seq(xs) => {
                for x in &xs[..xs.len() - 1] {
                    self.discard(x);
                }
                self.value_as(xs.last().unwrap(), want)
            }
            HK::Call(c) => self.call(e, c, want),
            HK::New { class, args } => {
                let ctor = self.hir.classes[*class as usize].ctor;
                if !self.el.new_direct(self.fid, *class, args) {
                    let k = self.b.ins().iconst(types::I32, CALL_CONSTRUCT as i64);
                    let a = self.b.ins().iconst(types::I64, *class as i64);
                    let r = self.interp_call(k, a, None, None, args, Vec::new());
                    return self.coerce(r, JV::Val, want);
                }
                let cls = self.b.ins().iconst(types::I64, std::rc::Rc::as_ptr(&self.classes[*class as usize]) as i64);
                let arena = self.arena_for(e as *const HExpr as usize).unwrap_or_else(|| self.b.ins().iconst(types::I64, 0));
                let call = self.b.ins().call(self.h.new, &[cls, arena]);
                let obj = self.b.inst_results(call)[0];
                let res = self.fresh(RV::new(obj));
                let mut vals = vec![self.ctx];
                if self.takes_rarena.contains(&ctor) {
                    vals.push(self.b.ins().iconst(types::I64, 0));
                }
                // `this` as the constructor takes it (an object, or a value
                // when untyped code leaves its type open).
                let tv = self.hir.funcs[ctor as usize].this_var.unwrap();
                let this = self.coerce(RV::new(obj), JV::Obj, var_jv(self.hir, tv));
                vals.extend(this.words());
                let pjs = self.param_jvs(ctor, false);
                let held = self.args(args, &pjs, &mut vals, false);
                let fr = self.func_ref(ctor);
                // The constructor's result (undefined) is not used.
                self.b.ins().call(fr, &vals);
                self.check_status();
                for h in held {
                    self.release(h);
                }
                res
            }
            HK::NewBuiltin(n, args) => {
                let k = self.b.ins().iconst(types::I32, CALL_NATIVE as i64);
                let a = self.b.ins().iconst(types::I64, *n as u16 as i64);
                let r = self.interp_call(k, a, None, None, args, Vec::new());
                self.coerce(r, JV::Val, want)
            }
            HK::Array(elems) => {
                // Number storage as the interpreter chooses it (see codegen's `HK::Array`).
                let numbers = match &e.ty {
                    Type::Array(t) if t.kind() == Kind::Num => true,
                    Type::Array(t) if matches!(**t, Type::Any | Type::Unknown) => {
                        elems.iter().all(|x| matches!(x, HElem::Expr(x) if x.ty.kind() == Kind::Num || matches!(x.kind, HK::Cast(ref y) if y.ty.kind() == Kind::Num)))
                    }
                    _ => false,
                };
                let n = self.b.ins().iconst(types::I32, numbers as i64);
                let arena = self.arena_for(e as *const HExpr as usize).unwrap_or_else(|| self.b.ins().iconst(types::I64, 0));
                let call = self.b.ins().call(self.h.arr_new, &[n, arena]);
                let a = self.b.inst_results(call)[0];
                let res = self.fresh(RV::new(a));
                for x in elems {
                    let HElem::Expr(x) = x else { unreachable!() };
                    let (tag, payload) = self.owned_words(x);
                    self.b.ins().call(self.h.arr_push, &[a, tag, payload]);
                }
                res
            }
            HK::Logical(op, a, b) => self.logical(*op, a, b, want),
            HK::Cond(c, a, b) => self.cond_expr(c, a, b, want),
            HK::Binary(HBin::Concat, a, b) => {
                let x = self.value_as(a, JV::Val);
                let x = self.hold_across(x, a, &[b]);
                let y = self.value_as(b, JV::Val);
                let p = self.op(OP_CONCAT, x.t.unwrap(), x.v, Some(y));
                let r = self.helper_val(p);
                self.release(x);
                self.release(y);
                self.coerce(r, JV::Val, want)
            }
            HK::Unary(op @ (HUnary::ToStr | HUnary::TypeOf), x) => {
                let x = self.value_as(x, JV::Val);
                let x = if x.owned || matches!(op, HUnary::TypeOf) { x } else { self.hold(x) };
                let code = if matches!(op, HUnary::ToStr) { OP_TO_STR } else { OP_TYPEOF };
                let p = self.op(code, x.t.unwrap(), x.v, None);
                let r = self.helper_val(p);
                self.release(x);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!("checked by Elig"),
        }
    }

    /// Read a variable, field, element or property, as a `jv(ty)`
    /// (borrowed, unless it had to be taken).
    fn read(&mut self, p: &Place, ty: &Type) -> RV {
        let want = jv(ty).unwrap();
        let counted = want.jk().counted();
        match p {
            Place::Var(v) => {
                let r = self.read_var(*v);
                self.coerce(r, var_jv(self.hir, *v), want)
            }
            Place::StrLen { obj, .. } => {
                let o = self.value_as(obj, JV::Val);
                let bits = self.op(OP_STR_LEN, o.t.unwrap(), o.v, None);
                self.check_status();
                self.release(o);
                RV::new(self.b.ins().bitcast(types::F64, MemFlagsData::new(), bits))
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, None, ERR_READ);
                let r = self.load_field(o.v, *slot, want);
                // A field of an object this code owns only temporarily (a
                // call's result) must be taken before the object goes.
                if o.owned && counted {
                    self.inc_rv(r);
                    let r = self.fresh(r);
                    self.release(o);
                    return r;
                }
                self.release(o);
                r
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index]);
                let i = self.expr(index);
                let r = self.arr_get(a.v, i, want);
                if a.owned && counted {
                    self.inc_rv(r);
                    let r = self.fresh(r);
                    self.release(a);
                    return r;
                }
                self.release(a);
                r
            }
            Place::ArrLen { obj, .. } => {
                let a = self.value_as(obj, JV::Arr);
                let v = self.arr_len(a.v);
                self.release(a);
                RV::new(v)
            }
            Place::Prop { obj, name, .. } => {
                // A getter may run: the object must stay alive.
                let o = self.value_as(obj, JV::Val);
                let o = if is_var_read(self.hir, obj) { o } else { self.hold(o) };
                let s = self.b.ins().iconst(types::I64, site(name) as *const _ as i64);
                let call = self.b.ins().call(self.h.get_prop, &[s, o.t.unwrap(), o.v]);
                let p = self.b.inst_results(call)[0];
                let r = self.helper_val(p);
                self.release(o);
                self.coerce(r, JV::Val, want)
            }
            Place::DynIndex { obj, key, .. } => {
                // A getter may run: the object and key must stay alive.
                let o = self.value_as(obj, JV::Val);
                let o = if o.owned || is_var_read(self.hir, obj) { o } else { self.hold(o) };
                let k = self.value_as(key, JV::Val);
                let k = if k.owned || is_var_read(self.hir, key) { k } else { self.hold(k) };
                let call = self.b.ins().call(self.h.get_dyn, &[o.t.unwrap(), o.v, k.t.unwrap(), k.v]);
                let p = self.b.inst_results(call)[0];
                let r = self.helper_val(p);
                self.release(o);
                self.release(k);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!(),
        }
    }

    /// `p = x`: the stored value (borrowed from where it was stored), as a `want`.
    fn assign(&mut self, p: &Place, x: &HExpr, want: JV) -> RV {
        let xj = jv(&x.ty).unwrap();
        match p {
            Place::Var(v) if self.borrowed.contains(v) => {
                let vj = var_jv(self.hir, *v);
                let r = self.value_as(x, vj);
                let r = self.disown(r);
                self.stats.borrowed += 1;
                self.set_var(*v, r);
                self.coerce(r, vj, want)
            }
            Place::Var(v) => {
                let vj = var_jv(self.hir, *v);
                let r = self.value_as(x, vj);
                let r = if vj.jk().counted() { self.own(r) } else { r };
                self.write_var(*v, r);
                self.coerce(RV { owned: false, ..r }, vj, want)
            }
            Place::Field { obj, slot, .. } => {
                let o = self.field_base(obj, Some(x), ERR_WRITE);
                let r = self.value_of(x);
                let r = if xj.jk().counted() { self.own(r) } else { r };
                self.store_field(o.v, *slot, xj, r);
                self.release(o);
                self.coerce(RV { owned: false, ..r }, xj, want)
            }
            Place::Index { obj, index, .. } => {
                let a = self.arr_base(obj, &[index, x]);
                let i = self.expr(index);
                let r = self.value_of(x);
                let r = if xj.jk().counted() { self.own(r) } else { r };
                self.arr_set(a.v, i, xj, r);
                self.release(a);
                self.coerce(RV { owned: false, ..r }, xj, want)
            }
            Place::Prop { obj, name, .. } => {
                let o = self.value_as(obj, JV::Val);
                let o = if !o.owned && (!is_var_read(self.hir, obj) || self.el.effects(x, true)) { self.hold(o) } else { o };
                let r = self.value_as(x, JV::Val);
                // One count for the property, one for the result (a setter may run).
                let r = self.hold(r);
                self.inc_rv(r);
                let s = self.b.ins().iconst(types::I64, site(name) as *const _ as i64);
                self.b.ins().call(self.h.set_prop, &[s, o.t.unwrap(), o.v, r.t.unwrap(), r.v]);
                self.check_status();
                self.release(o);
                self.coerce(r, JV::Val, want)
            }
            Place::DynIndex { obj, key, .. } => {
                let o = self.value_as(obj, JV::Val);
                let o = if !o.owned && (!is_var_read(self.hir, obj) || self.el.effects(x, true) || self.el.effects(key, true)) { self.hold(o) } else { o };
                let k = self.value_as(key, JV::Val);
                let k = if !k.owned && (!is_var_read(self.hir, key) || self.el.effects(x, true)) { self.hold(k) } else { k };
                let r = self.value_as(x, JV::Val);
                // One count for the element, one for the result.
                let r = self.hold(r);
                self.inc_rv(r);
                self.b.ins().call(self.h.set_dyn, &[o.t.unwrap(), o.v, k.t.unwrap(), k.v, r.t.unwrap(), r.v]);
                self.check_status();
                self.release(o);
                self.release(k);
                self.coerce(r, JV::Val, want)
            }
            _ => unreachable!(),
        }
    }

    /// What a compiled function takes (`this` first, if it has one and `this` is wanted).
    fn param_jvs(&self, g: FuncId, with_this: bool) -> Vec<JV> {
        let hg = &self.hir.funcs[g as usize];
        let this = if with_this { hg.this_var } else { None };
        this.iter().chain(hg.params.iter().map(|p| &p.var)).map(|v| var_jv(self.hir, *v)).collect()
    }

    /// Evaluate call arguments (as `pjs`) into `vals`. A borrowed argument
    /// must stay alive through the call: one read from a variable does (the
    /// callee can't change this function's variables) unless a later
    /// argument assigns; any other borrowed argument is owned for the call.
    /// Returns the references to release after the call.
    fn args(&mut self, args: &[HElem], pjs: &[JV], vals: &mut Vec<CV>, pure: bool) -> Vec<RV> {
        let mut held = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            let r = self.value_as(x, pjs[i]);
            if pjs[i].jk().counted() {
                let r = if self.must_hold(&r, x, &args[i + 1..], pure) { self.hold(r) } else { r };
                vals.extend(r.words());
                held.push(r);
            } else {
                vals.push(r.v);
            }
        }
        held
    }

    /// Must a borrowed argument `x` (value `r`) be owned for the call? A
    /// variable's value stays alive unless a later argument assigns; any
    /// other borrowed value, unless the callee is pure and no later
    /// argument has effects. A number or boolean has no count.
    fn must_hold(&self, r: &RV, x: &HExpr, later: &[HElem], pure: bool) -> bool {
        if r.owned || !jk(&x.ty).unwrap().counted() {
            return false;
        }
        let later_effects = |assigns_only: bool| later.iter().any(|a| matches!(a, HElem::Expr(y) if self.el.effects(y, assigns_only)));
        if is_var_read(self.hir, x) {
            later_effects(true)
        } else {
            !pure || later_effects(false)
        }
    }

    /// A call, its result as a `want`.
    fn call(&mut self, e: &HExpr, c: &HCall, want: JV) -> RV {
        if let Some(g) = self.el.direct_target(c) {
            return self.call_compiled(e, c, g, want);
        }
        if let Some(t) = self.el.virt_targets(self.fid, c) {
            return self.call_virtual(e, c, &t, want);
        }
        if self.lay.clo_caps.is_some()
            && let Some(k) = self.el.value_call(self.fid, c)
        {
            return self.call_value(c, k, want);
        }
        if let Callee::Prop(name) = &c.callee
            && c.this.is_some()
        {
            let r = self.call_named(c, name);
            return self.coerce(r, JV::Val, want);
        }
        match &c.callee {
            Callee::Native(Native::ArrPush) if self.el.inline_push(self.fid, c, want.jk()) => {
                let (t, [HElem::Expr(x)]) = (c.this.as_ref().unwrap(), &c.args[..]) else { unreachable!() };
                let a = self.arr_base(t, &[x]);
                let (tag, payload) = self.owned_words(x);
                let call = self.b.ins().call(self.h.arr_push, &[a.v, tag, payload]);
                let n = self.b.inst_results(call)[0];
                self.check_status();
                self.release(a);
                RV::new(n)
            }
            Callee::Native(n) if self.el.inline_math(self.fid, c, *n, want.jk()) => RV::new(self.math(*n, c)),
            _ => {
                let r = self.call_interp(c);
                self.coerce(r, JV::Val, want)
            }
        }
    }

    /// A call to a compiled function.
    fn call_compiled(&mut self, e: &HExpr, c: &HCall, g: FuncId, want: JV) -> RV {
        let hg = &self.hir.funcs[g as usize];
        let pure = self.el.pure.contains(&g);
        let mut vals = vec![self.ctx];
        let mut held = Vec::new();
        // Where the callee allocates a fresh result: a region of ours, or
        // our own result arena (none: the heap).
        if self.takes_rarena.contains(&g) {
            let a = self.arena_for(e as *const HExpr as usize).unwrap_or_else(|| self.b.ins().iconst(types::I64, 0));
            vals.push(a);
        }
        if let Some(tv) = hg.this_var {
            // An instance's methods take an object; static methods, their class (a value).
            let tj = var_jv(self.hir, tv);
            match &c.this {
                Some(t) => {
                    let r = self.value_as(t, tj);
                    let r = if self.must_hold(&r, t, &c.args, pure) { self.hold(r) } else { r };
                    if matches!(c.callee, Callee::Method { .. }) && tj.jk() == JK::Ref {
                        self.check_obj(r.v, ERR_METHOD);
                    }
                    vals.extend(r.words());
                    held.push(r);
                }
                None => {
                    let z = self.zero_rv(tj.jk());
                    vals.extend(z.words());
                }
            }
        }
        let pjs = self.param_jvs(g, false);
        held.extend(self.args(&c.args, &pjs, &mut vals, pure));
        let fr = self.func_ref(g);
        let call = self.b.ins().call(fr, &vals);
        let res = RV::of_words(&self.b.inst_results(call).to_vec());
        let rj = ret_jv(hg).unwrap();
        let res = if rj.jk().counted() { self.fresh(res) } else { res };
        self.check_status();
        for h in held {
            self.release(h);
        }
        self.coerce(res, rj, want)
    }

    /// A method call with several compiled implementations (`targets`:
    /// receiver class and function): compare the receiver's class with each
    /// and call the implementation directly. Any other receiver (possible
    /// only through unchecked casts) goes through the interpreter, as before.
    fn call_virtual(&mut self, e: &HExpr, c: &HCall, targets: &[(u32, FuncId)], want: JV) -> RV {
        let Callee::Method { vslot, .. } = c.callee else { unreachable!() };
        let g0 = targets[0].1;
        let pure = targets.iter().all(|(_, g)| self.el.pure.contains(g));
        // The receiver and arguments, once, in the kinds every implementation takes.
        let t = c.this.as_ref().unwrap();
        let this = self.value_as(t, JV::Obj);
        let this = if self.must_hold(&this, t, &c.args, pure) { self.hold(this) } else { this };
        self.check_obj(this.v, ERR_METHOD);
        let mut held = vec![this];
        let pjs = self.param_jvs(g0, false);
        let mut args: Vec<(JV, RV)> = Vec::new();
        for (i, a) in c.args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            let r = self.value_as(x, pjs[i]);
            let r = if pjs[i].jk().counted() && self.must_hold(&r, x, &c.args[i + 1..], pure) { self.hold(r) } else { r };
            if pjs[i].jk().counted() {
                held.push(r);
            }
            args.push((pjs[i], r));
        }
        let arena = if targets.iter().any(|(_, g)| self.takes_rarena.contains(g)) {
            Some(self.arena_for(e as *const HExpr as usize).unwrap_or_else(|| self.b.ins().iconst(types::I64, 0)))
        } else {
            None
        };
        // The receiver's class (its shape's class pointer).
        let shape = self.b.ins().load(types::I64, mf(), this.v, Object::SHAPE_OFFSET as i32);
        let cls = self.b.ins().load(types::I64, mf(), shape, self.lay.shape_class);
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.append_block_param(merge, types::I64);
        let mut impls: Vec<FuncId> = targets.iter().map(|(_, g)| *g).collect();
        impls.dedup();
        impls.sort();
        impls.dedup();
        let n_impls = impls.len();
        // Each implementation through the entry table (compiled lazily: a
        // null entry goes through the interpreter, which compiles it).
        let slow = self.b.create_block();
        for g in impls {
            let mut hit: Option<CV> = None;
            for (k, _) in targets.iter().filter(|(_, h)| *h == g) {
                let bits = class_bits(&self.classes[*k as usize]);
                let eq = self.b.ins().icmp_imm_u(IntCC::Equal, cls, bits);
                hit = Some(match hit {
                    Some(h) => self.b.ins().bor(h, eq),
                    None => eq,
                });
            }
            let (check_b, call_b, next) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
            self.b.ins().brif(hit.unwrap(), check_b, &[], next, &[]);
            self.b.seal_block(check_b);
            self.b.seal_block(next);
            self.b.switch_to_block(check_b);
            // A direct call when compiled already, or when there are few
            // implementations (compiled with this function); through the
            // table otherwise (each is compiled when first called).
            let direct = self.defined.contains(&g) || n_impls <= 4;
            let ptr = if direct {
                self.b.ins().jump(call_b, &[]);
                None
            } else {
                let ent = self.b.ins().iconst(types::I64, unsafe { self.entries.add(g as usize) } as i64);
                let ptr = self.b.ins().load(types::I64, mf(), ent, std::mem::offset_of!(NativeEntry, entry) as i32);
                let ready = self.b.ins().icmp_imm_u(IntCC::NotEqual, ptr, 0);
                self.b.ins().brif(ready, call_b, &[], slow, &[]);
                Some(ptr)
            };
            self.b.seal_block(call_b);
            self.b.switch_to_block(call_b);
            let mut vals = vec![self.ctx];
            if self.takes_rarena.contains(&g) {
                vals.push(arena.unwrap());
            }
            vals.push(this.v);
            for (pj, r) in &args {
                if pj.jk().counted() {
                    vals.extend(r.words());
                } else {
                    vals.push(r.v);
                }
            }
            let call = match ptr {
                Some(ptr) => {
                    let sref = self.b.import_signature(self.fdecls[&g].1.clone());
                    self.b.ins().call_indirect(sref, ptr, &vals)
                }
                None => {
                    let fr = self.func_ref(g);
                    self.b.ins().call(fr, &vals)
                }
            };
            let res = RV::of_words(&self.b.inst_results(call).to_vec());
            let (rt, rp) = self.value_words(ret_jv(&self.hir.funcs[g as usize]).unwrap(), res);
            self.b.ins().jump(merge, &[rt.into(), rp.into()]);
            self.b.switch_to_block(next);
        }
        self.b.ins().jump(slow, &[]);
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        // Any other receiver: through the interpreter, with the values.
        {
            use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
            let n = args.len();
            let argv = if n > 0 {
                let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
                self.b.ins().stack_addr(types::I64, slot, 0)
            } else {
                self.b.ins().iconst(types::I64, 0)
            };
            for (i, (pj, r)) in args.clone().into_iter().enumerate() {
                let (wt, wp) = self.value_words(pj, r);
                self.b.ins().store(mf(), wt, argv, (16 * i) as i32);
                self.b.ins().store(mf(), wp, argv, (16 * i + 8) as i32);
            }
            let (tt, tp) = self.value_words(JV::Obj, this);
            let kind = self.b.ins().iconst(types::I32, CALL_VSLOT as i64);
            let a = self.b.ins().iconst(types::I64, vslot as i64);
            let z = self.b.ins().iconst(types::I64, 0);
            let argc = self.b.ins().iconst(types::I32, n as i64);
            let call = self.b.ins().call(self.h.call, &[kind, a, z, tt, tp, argv, argc]);
            let p = self.b.inst_results(call)[0];
            let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
            self.b.ins().jump(merge, &[t.into(), p.into()]);
        }
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
        let ps = self.b.block_params(merge).to_vec();
        // Owned (a compiled result, or the interpreter's); numbers carry no count.
        let r = self.fresh(RV::val(ps[0], ps[1]));
        self.check_status();
        for h in held {
            self.release(h);
        }
        self.coerce(r, JV::Val, want)
    }

    /// A call through a function value whose type fixes the kinds (`key`):
    /// a compiled callee taking exactly those kinds is called directly
    /// (found in the entry table by the closure's function id); anything
    /// else goes through the interpreter.
    fn call_value(&mut self, c: &HCall, key: (Vec<JV>, JV), want: JV) -> RV {
        let Callee::Value(fv) = &c.callee else { unreachable!() };
        let (pjs, rj) = key.clone();
        let id = {
            let n = self.keys.borrow().len() as u32 + 1;
            *self.keys.borrow_mut().entry(key).or_insert(n)
        };
        let mut held = Vec::new();
        // As `call_interp` evaluates them: receiver, function, arguments.
        let this = c.this.as_ref().map(|t| {
            let r = self.value_as(t, JV::Val);
            let r = self.hold(r);
            held.push(r);
            r
        });
        let f = self.value_as(fv, JV::Val);
        let f = if self.must_hold(&f, fv, &c.args, false) { self.hold(f) } else { f };
        held.push(f);
        let mut args: Vec<(JV, RV)> = Vec::new();
        for (i, a) in c.args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            let r = self.value_as(x, pjs[i]);
            let r = if pjs[i].jk().counted() && self.must_hold(&r, x, &c.args[i + 1..], false) { self.hold(r) } else { r };
            if pjs[i].jk().counted() {
                held.push(r);
            }
            args.push((pjs[i], r));
        }
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.append_block_param(merge, types::I64);
        let slow = self.b.create_block();
        // A closure whose function is compiled with these kinds?
        let is_func = self.b.ins().icmp_imm_u(IntCC::Equal, f.t.unwrap(), self.lay.tag_func);
        let check = self.b.create_block();
        self.b.ins().brif(is_func, check, &[], slow, &[]);
        self.b.seal_block(check);
        self.b.switch_to_block(check);
        let fid = self.b.ins().load(types::I32, mf(), f.v, self.lay.clo_func);
        let fid = self.b.ins().uextend(types::I64, fid);
        let off = self.b.ins().imul_imm_u(fid, std::mem::size_of::<NativeEntry>() as i64);
        let base = self.b.ins().iconst(types::I64, self.entries as i64);
        let ent = self.b.ins().iadd(base, off);
        let k = self.b.ins().load(types::I32, mf(), ent, std::mem::offset_of!(NativeEntry, key) as i32);
        let ok = self.b.ins().icmp_imm_u(IntCC::Equal, k, id as i64);
        let fast = self.b.create_block();
        let adapt = self.b.create_block();
        self.b.ins().brif(ok, fast, &[], adapt, &[]);
        // Other kinds (a generic callback, say): the callee's adapter, if
        // it takes this many arguments.
        self.b.seal_block(adapt);
        self.b.switch_to_block(adapt);
        let arity = self.b.ins().load(types::I64, mf(), ent, std::mem::offset_of!(NativeEntry, arity) as i32);
        let has = self.b.ins().icmp_imm_u(IntCC::Equal, arity, args.len() as i64);
        let aptr = self.b.ins().load(types::I64, mf(), ent, std::mem::offset_of!(NativeEntry, adapter) as i32);
        let nonzero = self.b.ins().icmp_imm_u(IntCC::NotEqual, aptr, 0);
        let use_adapter = self.b.ins().band(has, nonzero);
        let via = self.b.create_block();
        self.b.ins().brif(use_adapter, via, &[], slow, &[]);
        self.b.seal_block(via);
        self.b.switch_to_block(via);
        {
            let mut sig = self.sig0.clone();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(types::I64));
            let mut vals = vec![self.ctx, f.v];
            for (pj, r) in args.clone() {
                let (wt, wp) = self.value_words(pj, r);
                vals.extend([wt, wp]);
                sig.params.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            }
            sig.returns.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            let sref = self.b.import_signature(sig);
            let call = self.b.ins().call_indirect(sref, aptr, &vals);
            let rs = self.b.inst_results(call).to_vec();
            self.b.ins().jump(merge, &[rs[0].into(), rs[1].into()]);
        }
        self.b.switch_to_block(fast);
        self.b.seal_block(fast);
        let ptr = self.b.ins().load(types::I64, mf(), ent, std::mem::offset_of!(NativeEntry, entry) as i32);
        let flags = self.b.ins().load(types::I32, mf(), ent, std::mem::offset_of!(NativeEntry, flags) as i32);
        let env = self.b.ins().load(types::I64, mf(), f.v, self.lay.clo_caps.unwrap());
        let zero = self.b.ins().iconst(types::I64, 0);
        let mut words: Vec<CV> = Vec::new();
        let mut tys: Vec<types::Type> = Vec::new();
        for (pj, r) in &args {
            if pj.jk().counted() {
                words.extend(r.words());
            } else {
                words.push(r.v);
            }
            tys.extend(cl_tys(pj.jk()));
        }
        let ptr_ty = types::I64;
        // One call per combination of the hidden arguments (cells, result arena).
        let blocks: Vec<_> = (0..4).map(|_| self.b.create_block()).collect();
        let mut table = cranelift_frontend::Switch::new();
        for (i, blk) in blocks.iter().enumerate().skip(1) {
            table.set_entry(i as u128, *blk);
        }
        table.emit(&mut self.b, flags, blocks[0]);
        for (i, blk) in blocks.iter().enumerate() {
            self.b.seal_block(*blk);
            self.b.switch_to_block(*blk);
            let mut sig = self.sig0.clone();
            let mut vals = vec![self.ctx];
            sig.params.push(AbiParam::new(ptr_ty));
            if i & 1 != 0 {
                vals.push(env);
                sig.params.push(AbiParam::new(ptr_ty));
            }
            if i & 2 != 0 {
                // The result goes to the heap.
                vals.push(zero);
                sig.params.push(AbiParam::new(ptr_ty));
            }
            vals.extend(&words);
            sig.params.extend(tys.iter().map(|t| AbiParam::new(*t)));
            sig.returns.extend(cl_tys(rj.jk()).into_iter().map(AbiParam::new));
            let sref = self.b.import_signature(sig);
            let call = self.b.ins().call_indirect(sref, ptr, &vals);
            let res = RV::of_words(&self.b.inst_results(call).to_vec());
            let (rt, rp) = self.value_words(rj, res);
            self.b.ins().jump(merge, &[rt.into(), rp.into()]);
        }
        // Anything else: through the interpreter, with the values.
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        {
            use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
            let n = args.len();
            let argv = if n > 0 {
                let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
                self.b.ins().stack_addr(types::I64, slot, 0)
            } else {
                self.b.ins().iconst(types::I64, 0)
            };
            for (i, (pj, r)) in args.clone().into_iter().enumerate() {
                let (wt, wp) = self.value_words(pj, r);
                self.b.ins().store(mf(), wt, argv, (16 * i) as i32);
                self.b.ins().store(mf(), wp, argv, (16 * i + 8) as i32);
            }
            let (tt, tp) = match this {
                Some(r) => (r.t.unwrap(), r.v),
                None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
            };
            let kind = self.b.ins().iconst(types::I32, CALL_VALUE as i64);
            let argc = self.b.ins().iconst(types::I32, n as i64);
            let call = self.b.ins().call(self.h.call, &[kind, f.t.unwrap(), f.v, tt, tp, argv, argc]);
            let p = self.b.inst_results(call)[0];
            let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
            self.b.ins().jump(merge, &[t.into(), p.into()]);
        }
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
        let ps = self.b.block_params(merge).to_vec();
        let r = self.fresh(RV::val(ps[0], ps[1]));
        self.check_status();
        for h in held {
            self.release(h);
        }
        self.coerce(r, JV::Val, want)
    }

    /// `recv.name(…)` looked up by name: an inline cache keyed on the
    /// receiver's shape and class calls the method's adapter directly; a
    /// miss calls through the interpreter and fills the cache (`h_call_named`).
    fn call_named(&mut self, c: &HCall, name: &Name) -> RV {
        let t = c.this.as_ref().unwrap();
        let this = self.value_as(t, JV::Val);
        let this = if self.must_hold(&this, t, &c.args, false) { self.hold(this) } else { this };
        let mut held = vec![this];
        let mut args = Vec::new();
        for (i, a) in c.args.iter().enumerate() {
            let HElem::Expr(x) = a else { unreachable!() };
            let r = self.value_as(x, JV::Val);
            let r = if self.must_hold(&r, x, &c.args[i + 1..], false) { self.hold(r) } else { r };
            held.push(r);
            args.push(r);
        }
        let cache: *mut NamedCache = Box::leak(Box::default());
        let cache_v = self.b.ins().iconst(types::I64, cache as i64);
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        self.b.append_block_param(merge, types::I64);
        let slow = self.b.create_block();
        let is_obj = self.b.ins().icmp_imm_u(IntCC::Equal, this.t.unwrap(), self.lay.tag_obj);
        let check = self.b.create_block();
        self.b.ins().brif(is_obj, check, &[], slow, &[]);
        self.b.seal_block(check);
        self.b.switch_to_block(check);
        let shape = self.b.ins().load(types::I64, mf(), this.v, Object::SHAPE_OFFSET as i32);
        let cs = self.b.ins().load(types::I64, mf(), cache_v, std::mem::offset_of!(NamedCache, shape) as i32);
        let hit = self.b.ins().icmp(IntCC::Equal, shape, cs);
        let fast = self.b.create_block();
        self.b.ins().brif(hit, fast, &[], slow, &[]);
        self.b.seal_block(fast);
        self.b.switch_to_block(fast);
        {
            let ad = self.b.ins().load(types::I64, mf(), cache_v, std::mem::offset_of!(NamedCache, adapter) as i32);
            let mut sig = self.sig0.clone();
            let mut vals = vec![self.ctx, this.t.unwrap(), this.v];
            sig.params.extend([AbiParam::new(types::I64), AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            for r in &args {
                vals.extend([r.t.unwrap(), r.v]);
                sig.params.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            }
            sig.returns.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            let sref = self.b.import_signature(sig);
            let call = self.b.ins().call_indirect(sref, ad, &vals);
            let rs = self.b.inst_results(call).to_vec();
            self.b.ins().jump(merge, &[rs[0].into(), rs[1].into()]);
        }
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        {
            use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
            let n = args.len();
            let argv = if n > 0 {
                let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
                self.b.ins().stack_addr(types::I64, slot, 0)
            } else {
                self.b.ins().iconst(types::I64, 0)
            };
            for (i, r) in args.iter().enumerate() {
                self.b.ins().store(mf(), r.t.unwrap(), argv, (16 * i) as i32);
                self.b.ins().store(mf(), r.v, argv, (16 * i + 8) as i32);
            }
            let s = self.b.ins().iconst(types::I64, site(name) as *const _ as i64);
            let argc = self.b.ins().iconst(types::I32, n as i64);
            let call = self.b.ins().call(self.h.call_named, &[s, cache_v, this.t.unwrap(), this.v, argv, argc]);
            let p = self.b.inst_results(call)[0];
            let t = self.b.ins().load(types::I64, mf(), self.ctx, CTX_RET_TAG);
            self.b.ins().jump(merge, &[t.into(), p.into()]);
        }
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
        let ps = self.b.block_params(merge).to_vec();
        let r = self.fresh(RV::val(ps[0], ps[1]));
        self.check_status();
        for h in held {
            self.release(h);
        }
        r
    }

    /// A call through the interpreter (`h_call`): an owned value.
    fn call_interp(&mut self, c: &HCall) -> RV {
        if std::env::var_os("RTS_JIT_DEBUG").is_some() {
            let name = |g: FuncId| self.hir.funcs[g as usize].name.to_string();
            let what = match &c.callee {
                Callee::Direct { func, .. } => format!("function {}", name(*func)),
                Callee::Method { class, vslot } => format!("method {}.{}", self.hir.classes[*class as usize].name, self.hir.classes[*class as usize].vtable[*vslot as usize].0),
                Callee::Prop(p) => format!("property {p}"),
                Callee::Value(_) => "a function value".into(),
                Callee::Native(n) => format!("built-in {}", n.name()),
            };
            eprintln!("rts jit: {} calls {what} through the interpreter", name(self.fid));
        }
        let mut held = Vec::new();
        // The receiver, then the function, then the arguments.
        let this = match &c.this {
            Some(t) => {
                let r = self.value_as(t, JV::Val);
                let r = if self.must_hold(&r, t, &c.args, false) || matches!(c.callee, Callee::Value(_)) { self.hold(r) } else { r };
                held.push(r);
                Some(r)
            }
            None => None,
        };
        let iconst32 = |s: &mut Self, x: u32| s.b.ins().iconst(types::I32, x as i64);
        let (kind, a, b) = match &c.callee {
            Callee::Direct { func, closure } if !self.hir.funcs[*func as usize].free.is_empty() => {
                let v = closure.unwrap();
                let r = self.read_var(v);
                let r = self.coerce(r, var_jv(self.hir, v), JV::Val);
                let r = if self.in_mem(v) || c.args.iter().any(|a| matches!(a, HElem::Expr(y) if self.el.effects(y, true))) { self.hold(r) } else { r };
                held.push(r);
                (iconst32(self, CALL_VALUE), r.t.unwrap(), Some(r.v))
            }
            Callee::Direct { func, .. } => (iconst32(self, CALL_FUNC), self.b.ins().iconst(types::I64, *func as i64), None),
            Callee::Method { vslot, .. } => (iconst32(self, CALL_VSLOT), self.b.ins().iconst(types::I64, *vslot as i64), None),
            Callee::Prop(name) => (iconst32(self, CALL_NAMED), self.b.ins().iconst(types::I64, site(name) as *const _ as i64), None),
            Callee::Native(n) => (iconst32(self, CALL_NATIVE), self.b.ins().iconst(types::I64, *n as u16 as i64), None),
            Callee::Value(f) => {
                let r = self.value_as(f, JV::Val);
                let r = if self.must_hold(&r, f, &c.args, false) { self.hold(r) } else { r };
                held.push(r);
                (iconst32(self, CALL_VALUE), r.t.unwrap(), Some(r.v))
            }
        };
        self.interp_call(kind, a, b, this, &c.args, held)
    }

    /// `h_call(kind, a, b, this, args…)`: an owned value. `held` are
    /// references to release after the call.
    fn interp_call(&mut self, kind: CV, a: CV, b: Option<CV>, this: Option<RV>, args: &[HElem], mut held: Vec<RV>) -> RV {
        use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
        let n = args.len();
        let argv = if n > 0 {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
            self.b.ins().stack_addr(types::I64, slot, 0)
        } else {
            self.b.ins().iconst(types::I64, 0)
        };
        for (i, x) in args.iter().enumerate() {
            let HElem::Expr(x) = x else { unreachable!() };
            let r = self.value_as(x, JV::Val);
            let r = if self.must_hold(&r, x, &args[i + 1..], false) { self.hold(r) } else { r };
            self.b.ins().store(mf(), r.t.unwrap(), argv, (16 * i) as i32);
            self.b.ins().store(mf(), r.v, argv, (16 * i + 8) as i32);
            held.push(r);
        }
        let (tt, tp) = match this {
            Some(r) => (r.t.unwrap(), r.v),
            None => (self.b.ins().iconst(types::I64, self.lay.tag_undef), self.b.ins().iconst(types::I64, 0)),
        };
        let b = b.unwrap_or_else(|| self.b.ins().iconst(types::I64, 0));
        let argc = self.b.ins().iconst(types::I32, n as i64);
        let call = self.b.ins().call(self.h.call, &[kind, a, b, tt, tp, argv, argc]);
        let p = self.b.inst_results(call)[0];
        let r = self.helper_val(p);
        for h in held {
            self.release(h);
        }
        r
    }

    fn math(&mut self, n: Native, c: &HCall) -> CV {
        let args: Vec<CV> = c.args.iter().map(|a| if let HElem::Expr(x) = a { self.expr(x) } else { unreachable!() }).collect();
        let ins = self.b.ins();
        match n {
            Native::MathFloor => ins.floor(args[0]),
            Native::MathCeil => ins.ceil(args[0]),
            Native::MathTrunc => ins.trunc(args[0]),
            Native::MathSqrt => ins.sqrt(args[0]),
            Native::MathAbs => ins.fabs(args[0]),
            _ => {
                let (h, i, rest) = if let Some(i) = MATH1.iter().position(|m| *m == n) {
                    (self.h.math1, i, &args[..1])
                } else {
                    (self.h.math2, MATH2.iter().position(|m| *m == n).unwrap(), &args[..2])
                };
                let idx = self.b.ins().iconst(types::I32, i as i64);
                let mut all = vec![idx];
                all.extend_from_slice(rest);
                let call = self.b.ins().call(h, &all);
                self.b.inst_results(call)[0]
            }
        }
    }
}

impl Lower<'_, '_> {
    /// The compiled function `g`, imported into this function on first use
    /// (importing every function into every function is quadratic).
    fn func_ref(&mut self, g: FuncId) -> FuncRef {
        if let Some(r) = self.funcs.get(&g) {
            return *r;
        }
        self.called.borrow_mut().push(g);
        let (id, sig) = &self.fdecls[&g];
        let signature = self.b.func.import_signature(sig.clone());
        let name = self.b.func.declare_imported_user_function(cranelift_codegen::ir::UserExternalName { namespace: 0, index: id.as_u32() });
        let r = self.b.func.import_function(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(name),
            signature,
            // Local functions in the same module.
            colocated: true,
            patchable: false,
        });
        self.funcs.insert(g, r);
        r
    }
}

/// A compiled function, for calls through function values: its typed
/// entry point, the key of the kinds it takes and returns (0: none), and
/// whether it takes cells (1) and a result arena (2).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(super) struct NativeEntry {
    entry: u64,
    key: u32,
    flags: u32,
    /// The adapter taking every argument as a value (see `build_adapter`)
    /// and its parameter count (0 address: none).
    adapter: u64,
    arity: u64,
    /// For a method: its adapter taking `this` and every argument as values.
    pub(super) madapter: u64,
}

/// The entry table, for helpers (see `h_call_named`).
pub(super) static mut ENTRIES: *const NativeEntry = std::ptr::null();

impl NativeEntry {
    pub(super) fn method(fid: u32, argc: usize) -> u64 {
        let e = unsafe { ENTRIES };
        if e.is_null() {
            return 0;
        }
        let e = unsafe { &*e.add(fid as usize) };
        if e.madapter != 0 && e.arity == argc as u64 { e.madapter } else { 0 }
    }
}

/// Whether method `f` gets an adapter (an instance method taking an
/// object or any value, no rest or default parameters).
fn method_key(hir: &HProgram, f: FuncId) -> bool {
    let hf = &hir.funcs[f as usize];
    hf.class.is_some() && !hf.is_ctor && hf.free.is_empty()
        && hf.this_var.is_some_and(|tv| matches!(var_jv(hir, tv), JV::Obj | JV::Val))
        && !hf.params.iter().any(|p| p.rest || p.default.is_some())
        && ret_jv(hf).is_some()
}

/// What an adapter adapts (see `build_adapter`).
#[derive(Clone, Copy)]
enum Adapted {
    /// A function called through a closure: `(ctx, closure box, args…)`;
    /// the closure's cells are at `caps_off` in its box, if it takes them.
    Closure { env: bool, caps_off: i32 },
    /// Method `fid`, taking `this` as a `this`: `(ctx, this tag, this payload, args…)`.
    Method { fid: FuncId, this: JV },
}

/// An adapter for compiled function `callee` taking every argument as a
/// value, returning `(tag, payload)`, owned. Each argument is checked and
/// converted to the kind the function takes (as `JitFn::write_arg` does);
/// any mismatch makes the call through the interpreter instead (`h_call`).
#[allow(clippy::too_many_arguments)]
fn build_adapter(mut b: FunctionBuilder, lay: &Layout, callee: FuncRef, h_call: FuncRef, what: Adapted, pjs: &[JV], rj: JV, rarena: bool, fc: cranelift_codegen::isa::TargetFrontendConfig) {
    use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let ps = b.block_params(entry).to_vec();
    let c = ps[0];
    let first = if matches!(what, Adapted::Method { .. }) { 3 } else { 2 };
    let words: Vec<(CV, CV)> = (0..pjs.len()).map(|i| (ps[first + 2 * i], ps[first + 1 + 2 * i])).collect();
    let slow = b.create_block();
    let mut vals = vec![c];
    // Convert (t, p) to a `pj`, or go to `slow`.
    let convert = |b: &mut FunctionBuilder, pj: JV, t: CV, p: CV, vals: &mut Vec<CV>| {
        let is = |b: &mut FunctionBuilder, tag: i64| b.ins().icmp_imm_u(IntCC::Equal, t, tag);
        let need = |b: &mut FunctionBuilder, ok: CV| {
            let next = b.create_block();
            b.ins().brif(ok, next, &[], slow, &[]);
            b.seal_block(next);
            b.switch_to_block(next);
        };
        match pj {
            JV::Val => vals.extend([t, p]),
            JV::Num => {
                let ok = is(b, lay.tag_num);
                need(b, ok);
                vals.push(b.ins().bitcast(types::F64, MemFlagsData::new(), p));
            }
            JV::Bool => {
                let ok = is(b, lay.tag_bool);
                need(b, ok);
                vals.push(b.ins().ireduce(types::I8, p));
            }
            // A reference: the box, or 0 for null and 1 for undefined.
            JV::Obj | JV::Arr | JV::Nil => {
                let null = is(b, lay.tag_null);
                let undef = is(b, lay.tag_undef);
                let nil = b.ins().bor(null, undef);
                let ok = match pj {
                    JV::Obj => {
                        let o = is(b, lay.tag_obj);
                        b.ins().bor(nil, o)
                    }
                    JV::Arr => {
                        let a = is(b, lay.tag_arr);
                        b.ins().bor(nil, a)
                    }
                    _ => nil,
                };
                need(b, ok);
                let one = b.ins().iconst(types::I64, 1);
                let zero = b.ins().iconst(types::I64, 0);
                let nv = b.ins().select(undef, one, zero);
                vals.push(b.ins().select(nil, nv, p));
            }
        }
    };
    match what {
        Adapted::Closure { env, caps_off } => {
            if env {
                vals.push(b.ins().load(types::I64, mf(), ps[1], caps_off));
            }
            if rarena {
                vals.push(b.ins().iconst(types::I64, 0));
            }
        }
        Adapted::Method { this, .. } => {
            if rarena {
                vals.push(b.ins().iconst(types::I64, 0));
            }
            convert(&mut b, this, ps[1], ps[2], &mut vals);
        }
    }
    for (&pj, &(t, p)) in pjs.iter().zip(&words) {
        convert(&mut b, pj, t, p, &mut vals);
    }
    let call = b.ins().call(callee, &vals);
    let rs = b.inst_results(call).to_vec();
    let (rt, rp) = match rj {
        JV::Val => (rs[0], rs[1]),
        JV::Num => (b.ins().iconst(types::I64, lay.tag_num), b.ins().bitcast(types::I64, MemFlagsData::new(), rs[0])),
        JV::Bool => (b.ins().iconst(types::I64, lay.tag_bool), b.ins().uextend(types::I64, rs[0])),
        _ => {
            let v = rs[0];
            let heap = b.ins().iconst(types::I64, if rj == JV::Arr { lay.tag_arr } else { lay.tag_obj });
            let null = b.ins().iconst(types::I64, lay.tag_null);
            let undef = b.ins().iconst(types::I64, lay.tag_undef);
            let is_null = b.ins().icmp_imm_u(IntCC::Equal, v, 0);
            let is_undef = b.ins().icmp_imm_u(IntCC::Equal, v, 1);
            let t = b.ins().select(is_undef, undef, heap);
            (b.ins().select(is_null, null, t), v)
        }
    };
    b.ins().return_(&[rt, rp]);
    // A mismatch: the call through the interpreter, with the values.
    b.seal_block(slow);
    b.switch_to_block(slow);
    let n = words.len();
    let argv = if n > 0 {
        let slot = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (16 * n) as u32, 4));
        b.ins().stack_addr(types::I64, slot, 0)
    } else {
        b.ins().iconst(types::I64, 0)
    };
    for (i, &(t, p)) in words.iter().enumerate() {
        b.ins().store(mf(), t, argv, (16 * i) as i32);
        b.ins().store(mf(), p, argv, (16 * i + 8) as i32);
    }
    let argc = b.ins().iconst(types::I32, n as i64);
    let args = match what {
        Adapted::Closure { .. } => {
            let kind = b.ins().iconst(types::I32, CALL_VALUE as i64);
            let ft = b.ins().iconst(types::I64, lay.tag_func);
            let ut = b.ins().iconst(types::I64, lay.tag_undef);
            let z = b.ins().iconst(types::I64, 0);
            [kind, ft, ps[1], ut, z, argv, argc]
        }
        Adapted::Method { fid, .. } => {
            let kind = b.ins().iconst(types::I32, CALL_FUNC as i64);
            let f = b.ins().iconst(types::I64, fid as i64);
            let z = b.ins().iconst(types::I64, 0);
            [kind, f, z, ps[1], ps[2], argv, argc]
        }
    };
    let call = b.ins().call(h_call, &args);
    let p = b.inst_results(call)[0];
    let t = b.ins().load(types::I64, mf(), c, CTX_RET_TAG);
    b.ins().return_(&[t, p]);
    b.finalize(fc);
}

/// The kinds a function without `this` takes and returns, as a call
/// through a function value would pass them (None: not callable that way).
fn value_key(hir: &HProgram, f: FuncId) -> Option<(Vec<JV>, JV)> {
    let hf = &hir.funcs[f as usize];
    if hf.this_var.is_some() || hf.params.iter().any(|p| p.rest || p.default.is_some()) {
        return None;
    }
    Some((hf.params.iter().map(|p| var_jv(hir, p.var)).collect(), ret_jv(hf)?))
}

/// Whether a loop can allocate (so the heap can grow while it runs): it
/// calls, builds objects, arrays, closures or strings. A loop that only
/// does arithmetic, comparisons and reads needs no GC safepoint.
fn loop_may_allocate<'a>(body: &[HStmt], exprs: impl Iterator<Item = &'a HExpr>) -> bool {
    fn allocates(e: &HExpr) -> bool {
        let scalar = matches!(jv(&e.ty), Some(JV::Num | JV::Bool));
        match &e.kind {
            HK::Num(_) | HK::Bool(_) | HK::Null | HK::Undefined | HK::Read(_) | HK::Logical(..) | HK::Cond(..) | HK::Cast(_) | HK::Seq(_)
            | HK::InstanceOf(..) | HK::ClassRef(_) | HK::Assign(..) => false,
            HK::Unary(..) | HK::Binary(..) | HK::Compound(..) | HK::Update { .. } => !scalar,
            _ => true,
        }
    }
    fn any(e: &HExpr) -> bool {
        allocates(e) || children(e).into_iter().any(any)
    }
    exprs.into_iter().any(any) || any_expr(body, &mut allocates) || any_block(body, &mut |ss| ss.iter().any(|s| matches!(s, HStmt::MakeClosure(..) | HStmt::Try { .. })))
}

/// An expression whose value is read from somewhere that holds it (a
/// variable, field or element), not made or handed over by a call.
fn borrowed_source(x: &HExpr) -> bool {
    match &x.kind {
        HK::Null | HK::Undefined | HK::Num(_) | HK::Bool(_) => true,
        HK::Cast(y) => borrowed_source(y),
        HK::Cond(_, a, b) | HK::Logical(_, a, b) => borrowed_source(a) && borrowed_source(b),
        HK::Read(p) => matches!(**p, Place::Var(_) | Place::Field { .. } | Place::Index { .. }),
        _ => false,
    }
}

/// A class's pointer as its instances' shapes hold it.
fn class_bits(c: &std::rc::Rc<ClassRt>) -> i64 {
    std::rc::Rc::as_ptr(c) as i64
}

pub(super) fn strip_cast(e: &HExpr) -> &HExpr {
    match &e.kind {
        HK::Cast(x) => strip_cast(x),
        _ => e,
    }
}

/// The `JK`s a compiled function takes: `this` first, if it has one.
pub(super) fn param_jks(el: &Elig, hf: &HFunc) -> Vec<JK> {
    hf.this_var.iter().chain(hf.params.iter().map(|p| &p.var)).map(|v| el.vjk(*v).unwrap()).collect()
}

/// The machine words a compiled function takes (after the context).
pub(super) fn param_tys(el: &Elig, hf: &HFunc) -> Vec<types::Type> {
    param_jks(el, hf).into_iter().flat_map(cl_tys).collect()
}

pub(super) fn var_jv(hir: &HProgram, v: VarId) -> JV {
    jv(&hir.vars[v as usize].ty).unwrap()
}

/// The native-code compiler, kept for the program's lifetime: functions are
/// declared up front and defined (compiled) lazily, on their second call
/// (`RTS_JIT_EAGER`: all at start), each with the functions it calls directly.
pub(super) struct Jit {
    hir: &'static HProgram,
    esc: &'static crate::escape::EscapeInfo,
    el: Elig<'static>,
    takes_rarena: FxHashSet<FuncId>,
    lay: Layout,
    m: JITModule,
    h_ids: Vec<ClFuncId>,
    call_idx: usize,
    /// Each eligible function's typed definition and entry trampoline.
    ids: FxHashMap<FuncId, (ClFuncId, ClFuncId)>,
    fdecls: FxHashMap<FuncId, (ClFuncId, Signature)>,
    entries: *mut NativeEntry,
    keys: RefCell<FxHashMap<(Vec<JV>, JV), u32>>,
    sig0: Signature,
    vars_of: FxHashMap<FuncId, Vec<VarId>>,
    globals: FxHashMap<VarId, u32>,
    classes: Vec<std::rc::Rc<ClassRt>>,
    stats: RcStats,
    defined: FxHashSet<FuncId>,
    /// Each function's `JitFn` (in the program), by id; null if not eligible.
    jitfns: Vec<*const JitFn>,
    debug: bool,
    /// A definition failed: nothing more is compiled.
    broken: bool,
}

static mut JIT: *mut Jit = std::ptr::null_mut();

/// Calls before a function without loops is compiled (the second call
/// compiles it). A function with a loop is compiled on its first call: it
/// may run long, and a call already running stays in the interpreter.
const WARM_CALLS: u32 = 1;

/// Called by the interpreter before calling `jf`: true if its code is ready
/// (compiling it, and what it calls directly, once it is warm).
#[inline]
pub(crate) fn ready(jf: &JitFn) -> bool {
    if jf.entry.get().is_some() {
        return true;
    }
    let n = jf.calls.get();
    if n < jf.warm {
        jf.calls.set(n + 1);
        return false;
    }
    let j = unsafe { JIT };
    if j.is_null() {
        return false;
    }
    unsafe { (*j).define(jf.fid) };
    jf.entry.get().is_some()
}

/// Set up the compiler and give every eligible function a (not yet
/// compiled) `JitFn`. Returns how many functions are eligible.
pub fn compile(hir: &'static HProgram, prog: &mut Program, esc: &'static crate::escape::EscapeInfo) -> usize {
    if !unsafe { ENABLED } {
        return 0;
    }
    let mut subclasses: Vec<Vec<u32>> = vec![Vec::new(); hir.classes.len()];
    for k in 0..hir.classes.len() as u32 {
        let mut c = Some(k);
        while let Some(x) = c {
            subclasses[x as usize].push(k);
            c = hir.classes[x as usize].parent;
        }
    }
    // Eligible functions: a fixpoint, since a call must go to a compiled function.
    let mut el = Elig { hir, set: (0..hir.funcs.len() as FuncId).collect(), pure: FxHashSet::default(), subclasses };
    loop {
        let keep: FxHashSet<FuncId> = el.set.iter().copied().filter(|&f| el.func_ok(f)).collect();
        if keep.len() == el.set.len() {
            break;
        }
        el.set = keep;
    }
    el.compute_pure();
    // Functions with allocations (or calls) whose object is only returned
    // take the arena their caller gives for the result.
    let takes_rarena: FxHashSet<FuncId> = if arenas_on() {
        el.set.iter().copied().filter(|&f| any_expr(&hir.funcs[f as usize].body, &mut |e| esc.result.contains(&(e as *const HExpr as usize)))).collect()
    } else {
        FxHashSet::default()
    };
    let debug = std::env::var_os("RTS_JIT_DEBUG").is_some();
    if el.set.is_empty() {
        return 0;
    }
    let lay = Layout::new();
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    // The verifier checks Cranelift's own invariants; keep it for debug builds.
    if !cfg!(debug_assertions) && !debug {
        flags.set("enable_verifier", "false").unwrap();
    }
    let Ok(isa) = cranelift_native::builder().and_then(|b| b.finish(settings::Flags::new(flags)).map_err(|_| "isa")) else {
        return 0;
    };
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let helpers = Helpers::table();
    for (n, p, _, _) in &helpers {
        jb.symbol(*n, *p);
    }
    let mut m = JITModule::new(jb);
    let ptr = m.target_config().pointer_type();
    let h_ids: Vec<ClFuncId> = helpers
        .iter()
        .map(|(n, _, params, ret)| {
            let mut s = m.make_signature();
            s.params.extend(params.iter().map(|t| AbiParam::new(*t)));
            s.returns.extend(ret.map(AbiParam::new));
            m.declare_function(n, Linkage::Import, &s).unwrap()
        })
        .collect();
    let call_idx = helpers.iter().position(|(n, ..)| *n == "rts_call").unwrap();

    // Declare every eligible function (typed) and its entry trampoline.
    let mut ids = FxHashMap::default();
    let mut fdecls = FxHashMap::default();
    let mut order: Vec<FuncId> = el.set.iter().copied().collect();
    order.sort();
    for &f in &order {
        let hf = &hir.funcs[f as usize];
        let mut s = m.make_signature();
        s.params.push(AbiParam::new(ptr));
        // A closure's cells come first, then the result arena.
        if !hf.free.is_empty() {
            s.params.push(AbiParam::new(ptr));
        }
        if takes_rarena.contains(&f) {
            s.params.push(AbiParam::new(ptr));
        }
        s.params.extend(param_tys(&el, hf).into_iter().map(AbiParam::new));
        s.returns.extend(cl_tys(ret_jk(hf).unwrap()).into_iter().map(AbiParam::new));
        let id = m.declare_function(&format!("f{f}"), Linkage::Local, &s).unwrap();
        let mut es = m.make_signature();
        es.params.extend([AbiParam::new(ptr), AbiParam::new(ptr)]);
        es.returns.push(AbiParam::new(types::I64));
        let eid = m.declare_function(&format!("e{f}"), Linkage::Local, &es).unwrap();
        fdecls.insert(f, (id, s));
        ids.insert(f, (id, eid));
    }
    // Every function's entry (filled in as functions are defined).
    let entries: *mut NativeEntry = Box::leak(vec![NativeEntry::default(); hir.funcs.len()].into_boxed_slice()).as_mut_ptr();
    let sig0 = m.make_signature();
    // Each function's variables.
    let mut vars_of: FxHashMap<FuncId, Vec<VarId>> = FxHashMap::default();
    for (v, vi) in hir.vars.iter().enumerate() {
        if let Some(o) = vi.owner
            && el.set.contains(&o)
        {
            vars_of.entry(o).or_default().push(v as VarId);
        }
    }
    // Global variables' indices, as codegen assigns them.
    let mut globals: FxHashMap<VarId, u32> = FxHashMap::default();
    let (mut n_gs, mut n_gv) = (0u32, 0u32);
    for (i, v) in hir.vars.iter().enumerate() {
        if v.owner.is_none() {
            if v.kind().is_scalar() {
                globals.insert(i as VarId, n_gs);
                n_gs += v.kind().width() as u32;
            } else {
                globals.insert(i as VarId, n_gv);
                n_gv += 1;
            }
        }
    }
    #[cfg(feature = "profile")]
    FN_NAMES.with(|n| {
        *n.borrow_mut() = hir
            .funcs
            .iter()
            .map(|hf| match hf.class {
                Some(c) => format!("{}.{}", hir.classes[c as usize].name, hf.name),
                None => hf.name.to_string(),
            })
            .collect()
    });
    // The interpreter's view: each eligible function's `JitFn`, no code yet.
    let mut jitfns = vec![std::ptr::null(); hir.funcs.len()];
    for &f in &order {
        let hf = &hir.funcs[f as usize];
        if let Some(fc) = prog.funcs.get_mut(f as usize).and_then(std::rc::Rc::get_mut) {
            let jf = Box::new(JitFn {
                fid: f,
                entry: std::cell::Cell::new(None),
                calls: std::cell::Cell::new(0),
                warm: if any_block(&hf.body, &mut |ss| ss.iter().any(|s| matches!(s, HStmt::Loop { .. } | HStmt::ForOf { .. } | HStmt::ForIn { .. }))) { 0 } else { WARM_CALLS },
                this: hf.this_var.map(|v| var_jv(hir, v)),
                env: !hf.free.is_empty(),
                rarena: takes_rarena.contains(&f),
                params: hf.params.iter().map(|p| var_jv(hir, p.var)).collect(),
                ret: ret_jv(hf).unwrap(),
            });
            jitfns[f as usize] = &*jf as *const JitFn;
            fc.jit = Some(jf);
        }
    }
    let n = order.len();
    let jit = Box::leak(Box::new(Jit {
        hir,
        esc,
        el,
        takes_rarena,
        lay,
        m,
        h_ids,
        call_idx,
        ids,
        fdecls,
        entries,
        keys: RefCell::default(),
        sig0,
        vars_of,
        globals,
        classes: prog.classes.clone(),
        stats: RcStats::default(),
        defined: FxHashSet::default(),
        jitfns,
        debug,
        broken: false,
    }));
    unsafe {
        ENTRIES = entries;
        JIT = jit;
    }
    if std::env::var_os("RTS_JIT_EAGER").is_some() {
        for f in order {
            jit.define(f);
        }
    }
    n
}

impl Jit {
    /// Compile `f` (if not yet), with every function it calls directly.
    fn define(&mut self, f: FuncId) {
        if self.broken || self.defined.contains(&f) || !self.ids.contains_key(&f) {
            return;
        }
        let mut batch = vec![f];
        let mut queued: FxHashSet<FuncId> = [f].into_iter().collect();
        let mut i = 0;
        let mut adapters: Vec<(FuncId, ClFuncId, bool)> = Vec::new();
        let mut ctx = self.m.make_context();
        let mut fbc = FunctionBuilderContext::new();
        while i < batch.len() {
            let g = batch[i];
            i += 1;
            let called = RefCell::new(Vec::new());
            match self.define_one(g, &mut ctx, &mut fbc, &called) {
                Ok(a) => adapters.extend(a),
                Err(e) => {
                    eprintln!("rts: native code disabled ({} failed to compile: {e})", self.hir.funcs[g as usize].name);
                    self.broken = true;
                    return;
                }
            }
            for c in called.into_inner() {
                if !self.defined.contains(&c) && queued.insert(c) {
                    batch.push(c);
                }
            }
        }
        if self.m.finalize_definitions().is_err() {
            self.broken = true;
            return;
        }
        let hir = self.hir;
        for &(g, aid, method) in &adapters {
            let e = unsafe { &mut *self.entries.add(g as usize) };
            let p = self.m.get_finalized_function(aid) as u64;
            if method {
                e.madapter = p;
            } else {
                e.adapter = p;
            }
            e.arity = hir.funcs[g as usize].params.len() as u64;
        }
        for &g in &batch {
            let (id, eid) = self.ids[&g];
            let hf = &hir.funcs[g as usize];
            let e = unsafe { &mut *self.entries.add(g as usize) };
            e.entry = self.m.get_finalized_function(id) as u64;
            e.flags = (!hf.free.is_empty()) as u32 | (self.takes_rarena.contains(&g) as u32) << 1;
            if let Some(key) = value_key(hir, g) {
                let n = self.keys.borrow().len() as u32 + 1;
                e.key = *self.keys.borrow_mut().entry(key).or_insert(n);
            }
            let jf = self.jitfns[g as usize];
            if !jf.is_null() {
                let p = self.m.get_finalized_function(eid);
                unsafe { (*jf).entry.set(Some(std::mem::transmute::<*const u8, Entry>(p))) };
            }
        }
        if self.debug {
            let names: Vec<String> = batch
                .iter()
                .map(|g| {
                    let hf = &hir.funcs[*g as usize];
                    match hf.class {
                        Some(c) => format!("{}.{}", hir.classes[c as usize].name, if hf.is_ctor { "constructor" } else { &hf.name }),
                        None => hf.name.to_string(),
                    }
                })
                .collect();
            eprintln!("rts jit: compiled {}: {}", names.len(), names.join(" "));
            eprintln!(
                "rts jit: count updates emitted so far: {} increments, {} decrements; {} borrowed uses needed none",
                self.stats.incs, self.stats.decs, self.stats.borrowed
            );
        }
    }

    /// Define `f`, its entry trampoline and its adapter (not finalized).
    /// The functions it calls directly are added to `called`.
    fn define_one(&mut self, f: FuncId, ctx: &mut cranelift_codegen::Context, fbc: &mut FunctionBuilderContext, called: &RefCell<Vec<FuncId>>) -> Result<Vec<(FuncId, ClFuncId, bool)>, String> {
        self.defined.insert(f);
        let hir = self.hir;
        let esc = self.esc;
        let (id, eid) = self.ids[&f];
        let hf = &hir.funcs[f as usize];
        let rk = ret_jk(hf).unwrap();
        let lay = &self.lay;
        let ptr = self.m.target_config().pointer_type();
        // The function itself.
        ctx.func.signature = self.fdecls[&f].1.clone();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, fbc);
            let hr: Vec<FuncRef> = self.h_ids.iter().map(|i| self.m.declare_func_in_func(*i, b.func)).collect();
            let h = Helpers::new(&hr);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let params: Vec<CV> = b.block_params(entry).to_vec();
            let c = params[0];
            // Depth check.
            let depth = b.ins().load(types::I32, mf(), c, 0);
            let d1 = b.ins().iadd_imm_s(depth, 1);
            b.ins().store(mf(), d1, c, 0);
            let over = b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, d1, MAX_DEPTH as i64);
            let (bail, body) = (b.create_block(), b.create_block());
            b.ins().brif(over, bail, &[], body, &[]);
            b.seal_block(bail);
            b.seal_block(body);
            b.switch_to_block(bail);
            let one = b.ins().iconst(types::I32, STATUS_OVERFLOW as i64);
            b.ins().store(mf(), one, c, 4);
            b.ins().store(mf(), depth, c, 0);
            let z = match rk {
                JK::Num => vec![b.ins().f64const(f64::NAN)],
                JK::Bool => vec![b.ins().iconst(types::I8, 0)],
                JK::Ref => vec![b.ins().iconst(types::I64, 1)],
                JK::Val => vec![b.ins().iconst(types::I64, lay.tag_undef), b.ins().iconst(types::I64, 0)],
            };
            b.ins().return_(&z);
            b.switch_to_block(body);
            let unwind = b.create_block();
            let el = &self.el;
            let mut lw = Lower {
                hir,
                el,
                fid: f,
                rk,
                rjv: ret_jv(hf).unwrap(),
                b,
                vars: FxHashMap::default(),
                tvars: FxHashMap::default(),
                cells: FxHashMap::default(),
                declared: FxHashSet::default(),
                env: (!hf.free.is_empty()).then(|| params[1]),
                rarena: self.takes_rarena.contains(&f).then(|| params[1 + !hf.free.is_empty() as usize]),
                esc,
                regions: Vec::new(),
                region_mark: None,
                takes_rarena: &self.takes_rarena,
                owned: vec![],
                borrowed: FxHashSet::default(),
                ctx: c,
                depth,
                unwind,
                pending: vec![],
                loops: vec![],
                iters: FxHashMap::default(),
                funcs: FxHashMap::default(),
                called,
                defined: &self.defined,
                fdecls: &self.fdecls,
                keys: &self.keys,
                entries: self.entries,
                sig0: &self.sig0,
                h,
                lay,
                classes: &self.classes,
                globals: &self.globals,
                stats: &mut self.stats,
            };
            // The tracing GC's safepoint at entry (deferred counting
            // settles at allocations instead: see `h_new`).
            if cfg!(feature = "gc") {
                lw.gc_poll();
            }
            // Parameters (borrowed, unless assigned: then owned).
            let mut assigned = FxHashSet::default();
            assigned_vars(&hf.body, &mut assigned);
            let pvars: Vec<VarId> = hf.this_var.iter().chain(hf.params.iter().map(|p| &p.var)).copied().collect();
            let mine: &[VarId] = self.vars_of.get(&f).map(|v| v.as_slice()).unwrap_or(&[]);
            let all: Vec<VarId> = pvars.iter().chain(mine).copied().collect();
            lw.borrowed = if crate::runtime::zct::deferred() {
                // Deferred counting: locals that only ever hold values read
                // from elsewhere take no counts (the stack keeps them; see
                // `zct`). Those given new values keep counting: borrowing
                // them would put every new value on the zero-count list.
                let mut assigns = Vec::new();
                collect_ref_assigns(&hf.body, &mut assigns);
                let given: FxHashSet<VarId> = assigns.iter().filter(|(_, x)| !borrowed_source(x)).map(|(v, _)| *v).collect();
                all.iter().copied().filter(|v| lw.el.vjk(*v).is_some_and(|k| k.counted()) && !hir.vars[*v as usize].captured && !lw.is_global(*v) && !given.contains(v)).collect()
            } else {
                el.borrowed_vars(f, &all)
            };
            // Captured variables: no cell yet (a block or `Init` makes it).
            for &v in mine {
                if hir.vars[v as usize].captured {
                    let x = lw.b.declare_var(types::I64);
                    let none = lw.b.ins().iconst(types::I64, 1);
                    lw.b.def_var(x, none);
                    lw.cells.insert(v, x);
                }
            }
            let mut words = params[1 + lw.env.is_some() as usize + lw.rarena.is_some() as usize..].iter().copied();
            // Unwinding frees the regions opened since entry.
            if arenas_on() && any_block(&hf.body, &mut |ss| esc.regions.contains(&(ss.as_ptr() as usize))) {
                let a = lw.b.ins().iconst(types::I64, (&raw const REGION_DEPTH) as i64);
                lw.region_mark = Some(lw.b.ins().load(types::I64, mf(), a, 0));
            }
            for v in &pvars {
                let k = lw.el.vjk(*v).unwrap();
                let r = if k.wide() {
                    let t = words.next().unwrap();
                    RV::val(t, words.next().unwrap())
                } else {
                    RV::new(words.next().unwrap())
                };
                if lw.cells.contains_key(v) {
                    // A captured parameter moves to a cell, which owns a copy.
                    lw.new_cell(*v);
                    lw.declared.insert(*v);
                    if k.counted() {
                        lw.inc_rv(r);
                    }
                    lw.mem_set(*v, RV { owned: true, ..r });
                    continue;
                }
                lw.def_var(*v, r);
                if k.counted() && assigned.contains(v) && !lw.borrowed.contains(v) {
                    lw.inc_rv(r);
                    lw.owned.push(*v);
                }
            }
            // Locals: declared here, so every path sees them (undefined at first).
            for &v in mine {
                if !pvars.contains(&v) && !lw.cells.contains_key(&v) {
                    lw.var(v);
                    if lw.el.vjk(v).unwrap().counted() && !lw.borrowed.contains(&v) {
                        lw.owned.push(v);
                    }
                }
            }
            lw.declare_iters(&hf.body);
            lw.stmts(&hf.body);
            // Falling off the end returns undefined.
            lw.release_vars(None);
            let z = lw.zero_rv(rk);
            lw.ret(z);
            // Unwinding: release the variables, return anything.
            lw.b.switch_to_block(lw.unwind);
            lw.b.seal_block(lw.unwind);
            lw.release_vars(None);
            if let Some(m) = lw.region_mark {
                lw.b.ins().call(lw.h.region_unwind, &[m]);
            }
            let z = lw.zero_rv(rk);
            lw.ret(z);
            lw.b.finalize(self.m.isa().frontend_config());
        }
        self.m.define_function(id, ctx).map_err(|e| format!("{e:?}"))?;
        self.m.clear_context(ctx);

        // The entry trampoline: unpack argument bits, call, pack the result.
        let mut es = self.m.make_signature();
        es.params.extend([AbiParam::new(ptr), AbiParam::new(ptr)]);
        es.returns.push(AbiParam::new(types::I64));
        ctx.func.signature = es;
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, fbc);
            let fr = self.m.declare_func_in_func(id, b.func);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let (c, argv) = (b.block_params(blk)[0], b.block_params(blk)[1]);
            let mut args = vec![c];
            let mut i = 0;
            if !hf.free.is_empty() {
                args.push(b.ins().load(types::I64, mf(), argv, 0));
                i = 1;
            }
            if self.takes_rarena.contains(&f) {
                args.push(b.ins().load(types::I64, mf(), argv, i * 8));
                i += 1;
            }
            for k in param_jks(&self.el, hf) {
                for _ in 0..cl_tys(k).len() {
                    let bits = b.ins().load(types::I64, mf(), argv, (i * 8) as i32);
                    i += 1;
                    args.push(match k {
                        JK::Num => b.ins().bitcast(types::F64, MemFlagsData::new(), bits),
                        JK::Bool => b.ins().ireduce(types::I8, bits),
                        JK::Ref | JK::Val => bits,
                    });
                }
            }
            let call = b.ins().call(fr, &args);
            let rs = b.inst_results(call).to_vec();
            let out = match rk {
                JK::Num => b.ins().bitcast(types::I64, MemFlagsData::new(), rs[0]),
                JK::Bool => b.ins().uextend(types::I64, rs[0]),
                JK::Ref => rs[0],
                JK::Val => {
                    b.ins().store(mf(), rs[0], c, CTX_RET_TAG);
                    rs[1]
                }
            };
            b.ins().return_(&[out]);
            b.finalize(self.m.isa().frontend_config());
        }
        self.m.define_function(eid, ctx).map_err(|e| format!("{e:?}"))?;
        self.m.clear_context(ctx);

        // The adapter taking values (calls through function values with
        // other kinds, and by-name method calls).
        let Some(caps_off) = lay.clo_caps else { return Ok(vec![]) };
        let what = if value_key(hir, f).is_some() {
            Adapted::Closure { env: !hf.free.is_empty(), caps_off }
        } else if method_key(hir, f) {
            Adapted::Method { fid: f, this: var_jv(hir, hf.this_var.unwrap()) }
        } else {
            return Ok(vec![]);
        };
        let mut sig = self.m.make_signature();
        sig.params.push(AbiParam::new(ptr));
        sig.params.push(AbiParam::new(types::I64));
        if matches!(what, Adapted::Method { .. }) {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..hf.params.len() {
            sig.params.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
        }
        sig.returns.extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
        let aid = self.m.declare_function(&format!("a{f}"), Linkage::Local, &sig).unwrap();
        ctx.func.signature = sig;
        {
            let b = FunctionBuilder::new(&mut ctx.func, fbc);
            let callee = self.m.declare_func_in_func(id, b.func);
            let h_call = self.m.declare_func_in_func(self.h_ids[self.call_idx], b.func);
            let pjs: Vec<JV> = hf.params.iter().map(|p| var_jv(hir, p.var)).collect();
            build_adapter(b, lay, callee, h_call, what, &pjs, ret_jv(hf).unwrap(), self.takes_rarena.contains(&f), self.m.isa().frontend_config());
        }
        self.m.define_function(aid, ctx).map_err(|e| format!("adapter: {e:?}"))?;
        self.m.clear_context(ctx);
        Ok(vec![(f, aid, matches!(what, Adapted::Method { .. }))])
    }
}
