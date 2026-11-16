//! Escape analysis: which allocations can live in a scope arena.
//!
//! An allocation (`new C(…)`, an object or array literal) is *scope-bound* if
//! no reference to it can outlive some block of its function. Such an object
//! is allocated in that block's arena and freed, with everything else in the
//! arena, when the block exits — no reference counting decides its lifetime.
//!
//! The analysis is flow-insensitive points-to over allocation sites, one
//! function at a time, with summaries for calls:
//!
//! - Every local variable has the set of sites it may hold; every site has
//!   the set of sites stored into it ("contents", field-insensitive).
//! - A site **escapes** if it may reach a global, a captured variable, a
//!   return value, a thrown or yielded value, an unknown object or callee, or
//!   another escaping site. Reads from objects may also produce values the
//!   analysis doesn't know about, so storing into what a read produced makes
//!   the stored value escape.
//! - A call to a known function uses its summary: for each parameter (and
//!   `this`), whether it escapes, whether it may be returned (or something
//!   reachable from it), and which other parameters' objects it is stored
//!   into. Summaries are computed for the whole program to a fixpoint. A
//!   virtual call joins the summaries of every override. Built-ins come from
//!   a table; anything not in it makes its arguments escape.
//! - A non-escaping site's **region** is the innermost block that contains
//!   the declarations of all variables that may hold it, and the regions of
//!   all sites it is stored into. If a loop body lies between the allocation
//!   and that block, the site stays on the heap: its arena would otherwise
//!   grow with every iteration.
//!
//! Generators and async functions are left alone (their frames outlive calls).

use rustc_hash::{FxHashMap, FxHashSet};

use crate::hir::*;
use crate::runtime::natives::Native;
use crate::types::Type;

/// The analysis result, keyed by addresses in the HIR that codegen walks.
#[derive(Default)]
pub struct EscapeInfo {
    /// Allocation expression (address of its `HExpr`) → its region (address
    /// of the block's first statement).
    pub sites: FxHashMap<usize, usize>,
    /// Blocks (address of their first statement) that need a region.
    pub regions: FxHashSet<usize>,
    /// Allocations and calls whose object is only ever returned: they go in
    /// the region the caller passed for the result (or the heap, if none).
    pub result: FxHashSet<usize>,
    /// Allocation sites seen, and how many are scope-bound (for `--dump`).
    pub total: usize,
}

/// What a callee does with one of its parameters (index 0 is `this`).
#[derive(Clone, Default, PartialEq, Debug)]
struct ParamSum {
    esc: bool,
    /// The parameter (or something reachable from it) may be returned.
    ret: bool,
    /// It is stored into these parameters' objects (or objects reachable from them).
    into: Vec<usize>,
}

#[derive(Clone, Default, PartialEq, Debug)]
struct Summary {
    p: Vec<ParamSum>,
    /// Every value returned is an object allocated by this call (or a
    /// primitive): the caller may choose the region it is allocated in.
    fresh: bool,
}

impl Summary {
    fn get(&self, i: usize) -> ParamSum {
        self.p.get(i).cloned().unwrap_or(ParamSum { esc: true, ret: true, into: vec![] })
    }
    fn all_escape(n: usize) -> Summary {
        Summary { p: vec![ParamSum { esc: true, ret: true, into: vec![] }; n], fresh: false }
    }
    fn join(&mut self, o: &Summary) {
        self.fresh &= o.fresh;
        let n = self.p.len().max(o.p.len());
        self.p.resize(n, ParamSum::default());
        for (i, s) in self.p.iter_mut().enumerate() {
            let x = o.get(i);
            if i >= o.p.len() {
                continue;
            }
            s.esc |= x.esc;
            s.ret |= x.ret;
            for t in x.into {
                if !s.into.contains(&t) {
                    s.into.push(t);
                }
            }
        }
    }
}

type Node = u32;

/// An abstract value: the sites/parameters it may be, and whether it may
/// also be something the analysis doesn't track.
#[derive(Clone, Default, Debug)]
struct AV {
    nodes: Vec<Node>,
    unknown: bool,
}

impl AV {
    fn unknown() -> AV {
        AV { nodes: vec![], unknown: true }
    }
    fn none() -> AV {
        AV::default()
    }
    fn union(mut self, o: AV) -> AV {
        for n in o.nodes {
            if !self.nodes.contains(&n) {
                self.nodes.push(n);
            }
        }
        self.unknown |= o.unknown;
        self
    }
}

struct Block {
    parent: Option<usize>,
    key: usize,
    loop_body: bool,
    /// Codegen gives this block a slot mark and `Release` (it can hold a region).
    regionable: bool,
}

/// Analysis state for one function.
struct Fa<'a> {
    hir: &'a HProgram,
    fid: FuncId,
    sums: &'a [Summary],
    site_ids: FxHashMap<usize, Node>,
    site_key: Vec<usize>,
    site_block: Vec<usize>,
    site_heap: Vec<bool>,
    /// The node is a call whose result is fresh (not an allocation).
    site_call: Vec<bool>,
    /// Something returned may be a value the analysis doesn't track.
    ret_unknown: bool,
    n_params: usize,
    pts: FxHashMap<VarId, Vec<Node>>,
    /// Variables that may hold values the analysis doesn't track.
    var_unknown: FxHashSet<VarId>,
    /// No fixpoint was reached: everything escapes.
    gave_up: bool,
    contents: FxHashMap<Node, Vec<Node>>,
    esc: FxHashSet<Node>,
    ret: FxHashSet<Node>,
    blocks: Vec<Block>,
    block_keys: FxHashMap<usize, usize>,
    cur: Vec<usize>,
    var_block: FxHashMap<VarId, usize>,
    loop_header: u32,
    changed: bool,
}

const PARAM_BASE: Node = 1 << 30;

impl<'a> Fa<'a> {
    fn new(hir: &'a HProgram, fid: FuncId, sums: &'a [Summary]) -> Fa<'a> {
        let hf = &hir.funcs[fid as usize];
        let mut fa = Fa {
            hir,
            fid,
            sums,
            site_ids: FxHashMap::default(),
            site_key: vec![],
            site_block: vec![],
            site_heap: vec![],
            site_call: vec![],
            ret_unknown: false,
            n_params: hf.params.len() + 1,
            pts: FxHashMap::default(),
            var_unknown: FxHashSet::default(),
            gave_up: false,
            contents: FxHashMap::default(),
            esc: FxHashSet::default(),
            ret: FxHashSet::default(),
            blocks: vec![Block { parent: None, key: hf.body.as_ptr() as usize, loop_body: false, regionable: false }],
            block_keys: FxHashMap::default(),
            cur: vec![0],
            var_block: FxHashMap::default(),
            loop_header: 0,
            changed: false,
        };
        if let Some(t) = hf.this_var {
            fa.pts.insert(t, vec![PARAM_BASE]);
            fa.var_block.insert(t, 0);
        }
        for (i, p) in hf.params.iter().enumerate() {
            fa.pts.insert(p.var, vec![PARAM_BASE + 1 + i as u32]);
            fa.var_block.insert(p.var, 0);
        }
        fa
    }

    fn is_param(n: Node) -> bool {
        n >= PARAM_BASE
    }

    fn local(&self, v: VarId) -> bool {
        let vi = &self.hir.vars[v as usize];
        vi.owner == Some(self.fid) && !vi.captured
    }

    fn add_to(set: &mut Vec<Node>, nodes: &[Node]) -> bool {
        let mut ch = false;
        for &n in nodes {
            if !set.contains(&n) {
                set.push(n);
                ch = true;
            }
        }
        ch
    }

    fn add_pts(&mut self, v: VarId, av: &AV) {
        if !self.local(v) {
            self.escape(av);
            return;
        }
        let set = self.pts.entry(v).or_default();
        if Self::add_to(set, &av.nodes) {
            self.changed = true;
        }
        if av.unknown && self.var_unknown.insert(v) {
            self.changed = true;
        }
    }

    fn escape(&mut self, av: &AV) {
        for &n in &av.nodes {
            if self.esc.insert(n) {
                self.changed = true;
            }
        }
    }

    /// `target.f = v` (or anything that stores `v` inside `target`).
    fn store_into(&mut self, target: &AV, v: &AV) {
        if target.unknown {
            self.escape(v);
        }
        for &t in &target.nodes {
            let set = self.contents.entry(t).or_default();
            if Self::add_to(set, &v.nodes) {
                self.changed = true;
            }
        }
    }

    /// Reading a property of `o`: what was stored into it, plus unknown values.
    fn read_from(&self, o: &AV) -> AV {
        let mut out = AV::unknown();
        for &n in &o.nodes {
            if Self::is_param(n) {
                // A parameter stands for everything reachable from it.
                if !out.nodes.contains(&n) {
                    out.nodes.push(n);
                }
            } else if let Some(c) = self.contents.get(&n) {
                for &x in c {
                    if !out.nodes.contains(&x) {
                        out.nodes.push(x);
                    }
                }
            }
        }
        out
    }

    /// `av` and everything reachable from it through contents.
    fn closure(&self, av: &AV) -> AV {
        let mut out = av.clone();
        let mut i = 0;
        while i < out.nodes.len() {
            if let Some(c) = self.contents.get(&out.nodes[i]) {
                for &x in c {
                    if !out.nodes.contains(&x) {
                        out.nodes.push(x);
                    }
                }
            }
            i += 1;
        }
        out
    }

    fn site(&mut self, e: &HExpr) -> Node {
        let key = e as *const HExpr as usize;
        if let Some(&n) = self.site_ids.get(&key) {
            return n;
        }
        let n = self.site_key.len() as Node;
        self.site_ids.insert(key, n);
        self.site_key.push(key);
        self.site_block.push(*self.cur.last().unwrap());
        self.site_heap.push(self.loop_header > 0);
        self.site_call.push(false);
        n
    }

    fn enter_block(&mut self, ss: &[HStmt], loop_body: bool) {
        let key = ss.as_ptr() as usize;
        let idx = match self.block_keys.get(&key) {
            Some(&i) if !ss.is_empty() => i,
            _ => {
                let i = self.blocks.len();
                self.blocks.push(Block { parent: self.cur.last().copied(), key, loop_body, regionable: !ss.is_empty() });
                if !ss.is_empty() {
                    self.block_keys.insert(key, i);
                }
                i
            }
        };
        self.cur.push(idx);
    }

    fn declare(&mut self, v: VarId) {
        let b = *self.cur.last().unwrap();
        self.var_block.entry(v).or_insert(b);
    }

    // ───────────── statements ─────────────

    fn block(&mut self, ss: &[HStmt], vars: &[VarId], loop_body: bool) {
        self.enter_block(ss, loop_body);
        for v in vars {
            self.declare(*v);
        }
        self.stmts(ss);
        self.cur.pop();
    }

    fn stmts(&mut self, ss: &[HStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &HStmt) {
        match s {
            HStmt::Expr(e) => {
                self.expr(e);
            }
            HStmt::Init(v, e) => {
                self.declare(*v);
                if let Some(e) = e {
                    let av = self.expr(e);
                    self.add_pts(*v, &av);
                }
            }
            HStmt::MakeClosure(v, _) => self.declare(*v),
            HStmt::Block(ss, vars) => self.block(ss, vars, false),
            HStmt::If(c, t, e) => {
                self.expr(c);
                self.block(t, &[], false);
                self.block(e, &[], false);
            }
            HStmt::Loop { test, body, update, .. } => {
                self.loop_header += 1;
                if let Some(t) = test {
                    self.expr(t);
                }
                self.loop_header -= 1;
                self.block(body, &[], true);
                self.loop_header += 1;
                if let Some(u) = update {
                    self.expr(u);
                }
                self.loop_header -= 1;
            }
            HStmt::ForOf { elem, iter, body, is_await, .. } => {
                let it = self.expr(iter);
                if *is_await || !matches!(iter.ty.non_null(), Type::Array(_) | Type::Tuple(_)) {
                    // A user iterator runs code with the iterable as `this`.
                    self.escape(&it);
                }
                self.declare(*elem);
                let el = self.read_from(&it);
                self.add_pts(*elem, &el);
                self.block(body, &[], true);
            }
            HStmt::ForIn { key, obj, body, .. } => {
                self.expr(obj);
                self.declare(*key);
                self.block(body, &[], true);
            }
            HStmt::Break(_) | HStmt::Continue(_) => {}
            HStmt::Return(e) => {
                if let Some(e) = e {
                    let av = self.expr(e);
                    if av.unknown && !self.ret_unknown {
                        self.ret_unknown = true;
                        self.changed = true;
                    }
                    for n in av.nodes {
                        if self.ret.insert(n) {
                            self.changed = true;
                        }
                    }
                }
            }
            HStmt::Throw(e) => {
                let av = self.expr(e);
                self.escape(&av);
            }
            HStmt::Try { block, catch_var, handler, finalizer } => {
                self.block(block, &[], false);
                if let Some(v) = catch_var {
                    self.declare(*v);
                }
                self.block(handler, &[], false);
                if let Some(f) = finalizer {
                    self.block(f, &[], false);
                }
            }
            HStmt::Switch { disc, cases, .. } => {
                self.expr(disc);
                for (t, body) in cases {
                    if let Some(t) = t {
                        self.expr(t);
                    }
                    self.stmts(body);
                }
            }
            HStmt::Labeled(_, s) => self.stmt(s),
        }
    }

    // ───────────── expressions ─────────────

    fn elems(&mut self, es: &[HElem]) -> Vec<AV> {
        es.iter()
            .map(|e| match e {
                HElem::Expr(x) => self.expr(x),
                HElem::Spread(x) => {
                    let av = self.expr(x);
                    self.read_from(&av)
                }
            })
            .collect()
    }

    fn expr(&mut self, e: &HExpr) -> AV {
        match &e.kind {
            HK::Num(_) | HK::BigInt(_) | HK::Bool(_) | HK::Str(_) | HK::Null | HK::Undefined => AV::none(),
            HK::ClassRef(_) | HK::NativeFn(_) | HK::Closure(_) => AV::unknown(),
            HK::Read(p) => self.read_place(p),
            HK::Array(es) => {
                let s = self.site(e);
                let me = AV { nodes: vec![s], unknown: false };
                for av in self.elems(es) {
                    self.store_into(&me, &av);
                }
                me
            }
            HK::Object(props) => {
                let s = self.site(e);
                let me = AV { nodes: vec![s], unknown: false };
                for p in props {
                    match p {
                        HProp::Init(_, x) => {
                            let av = self.expr(x);
                            self.store_into(&me, &av);
                        }
                        HProp::Spread(x) => {
                            let av = self.expr(x);
                            let inner = self.read_from(&av);
                            self.store_into(&me, &inner);
                        }
                        HProp::Computed(k, x) => {
                            let k = self.expr(k);
                            self.escape(&k);
                            let av = self.expr(x);
                            self.store_into(&me, &av);
                        }
                        HProp::Accessor { get, set, .. } => {
                            // Accessors run code with the object as `this`.
                            for f in get.iter().chain(set.iter()) {
                                self.expr(f);
                            }
                            self.escape(&me);
                        }
                    }
                }
                me
            }
            HK::New { class, args } => {
                let s = self.site(e);
                let me = AV { nodes: vec![s], unknown: false };
                if self.class_has_accessors(*class) {
                    self.escape(&me);
                }
                let ctor = self.hir.classes[*class as usize].ctor;
                let avs = self.elems(args);
                let sum = self.sums[ctor as usize].clone();
                self.apply(&sum, Some(me.clone()), avs, rest_at(&self.hir.funcs[ctor as usize]));
                me
            }
            HK::NewBuiltin(_, args) => {
                for av in self.elems(args) {
                    self.escape(&av);
                }
                AV::unknown()
            }
            HK::Unary(op, x) => {
                let av = self.expr(x);
                if matches!(op, HUnary::ToNum | HUnary::ToStr | HUnary::NegDyn | HUnary::BitNotDyn) {
                    // valueOf/toString may run with the object as `this`.
                    self.escape(&av);
                }
                AV::none()
            }
            HK::Binary(op, a, b) => {
                let x = self.expr(a);
                let y = self.expr(b);
                if matches!(op, HBin::LooseEq(_) | HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_) | HBin::DynCmp(_)) {
                    self.escape(&x);
                    self.escape(&y);
                }
                AV::none()
            }
            HK::Logical(_, a, b) => {
                let x = self.expr(a);
                let y = self.expr(b);
                x.union(y)
            }
            HK::Cond(c, a, b) => {
                self.expr(c);
                let x = self.expr(a);
                let y = self.expr(b);
                x.union(y)
            }
            HK::Assign(p, v) => {
                let av = self.expr(v);
                self.write_place(p, &av);
                av
            }
            HK::Compound(p, op, v) => {
                let cur = self.read_place(p);
                let av = self.expr(v);
                if matches!(op, HBin::LooseEq(_) | HBin::DynAdd | HBin::DynNum(_) | HBin::DynNumeric(_) | HBin::DynCmp(_)) {
                    self.escape(&cur);
                    self.escape(&av);
                }
                self.write_place(p, &AV::none());
                AV::none()
            }
            HK::LogicalAssign(p, _, v) => {
                let cur = self.read_place(p);
                let av = self.expr(v);
                self.write_place(p, &av);
                cur.union(av)
            }
            HK::Update { place, .. } => {
                self.read_place(place);
                self.write_place(place, &AV::none());
                AV::none()
            }
            HK::Call(c) => self.call(e, c),
            HK::InstanceOf(x, _) => {
                self.expr(x);
                AV::none()
            }
            HK::Cast(x) | HK::OptChain(x) => self.expr(x),
            HK::Seq(xs) => {
                let mut last = AV::none();
                for x in xs {
                    last = self.expr(x);
                }
                last
            }
            HK::Delete(o, k) => {
                self.expr(o);
                self.expr(k);
                AV::none()
            }
            HK::Await(x) | HK::Yield(x) | HK::YieldStar(x) => {
                let av = self.expr(x);
                self.escape(&av);
                AV::unknown()
            }
            HK::StmtExpr(ss, x) => {
                self.stmts(ss);
                let av = self.expr(x);
                self.escape(&av);
                AV::unknown()
            }
        }
    }

    /// Accessors run user code with the instance as `this`, and a property
    /// read through a structural type may reach one: such instances stay on
    /// the heap.
    fn class_has_accessors(&self, c: crate::types::ClassId) -> bool {
        self.hir.classes[c as usize].vtable.iter().any(|(n, _)| n.starts_with("get ") || n.starts_with("set "))
    }

    fn read_place(&mut self, p: &Place) -> AV {
        match p {
            Place::Var(v) => {
                if self.local(*v) {
                    AV { nodes: self.pts.get(v).cloned().unwrap_or_default(), unknown: self.var_unknown.contains(v) }
                } else {
                    AV::unknown()
                }
            }
            Place::Field { obj, .. } | Place::Prop { obj, .. } => {
                let o = self.expr(obj);
                self.read_from(&o)
            }
            Place::Index { obj, index, .. } | Place::DynIndex { obj, key: index, .. } => {
                let o = self.expr(obj);
                self.expr(index);
                self.read_from(&o)
            }
            Place::ArrLen { obj, .. } | Place::StrLen { obj, .. } => {
                self.expr(obj);
                AV::none()
            }
            Place::Accessor { obj, .. } => {
                let o = self.expr(obj);
                self.escape(&o);
                AV::unknown()
            }
        }
    }

    fn write_place(&mut self, p: &Place, v: &AV) {
        match p {
            Place::Var(var) => self.add_pts(*var, v),
            Place::Field { obj, .. } | Place::Prop { obj, .. } => {
                let o = self.expr(obj);
                self.store_into(&o, v);
            }
            Place::Index { obj, index, .. } | Place::DynIndex { obj, key: index, .. } => {
                let o = self.expr(obj);
                self.expr(index);
                self.store_into(&o, v);
            }
            Place::ArrLen { obj, .. } | Place::StrLen { obj, .. } => {
                self.expr(obj);
            }
            Place::Accessor { obj, .. } => {
                let o = self.expr(obj);
                self.escape(&o);
                self.escape(v);
            }
        }
    }

    fn call(&mut self, e: &HExpr, c: &HCall) -> AV {
        let this = c.this.as_ref().map(|t| self.expr(t));
        let spread = c.args.iter().any(|a| matches!(a, HElem::Spread(_)));
        let args = self.elems(&c.args);
        let result = match &c.callee {
            Callee::Direct { func, .. } if !spread => {
                let sum = self.sums[*func as usize].clone();
                let r = self.apply(&sum, this, args, rest_at(&self.hir.funcs[*func as usize]));
                (r, sum.fresh)
            }
            Callee::Method { class, vslot } if !spread => {
                let mut sum: Option<Summary> = None;
                let mut rest = None;
                for (k, hc) in self.hir.classes.iter().enumerate() {
                    if !is_subclass(self.hir, k as u32, *class) {
                        continue;
                    }
                    if let Some((_, f)) = hc.vtable.get(*vslot as usize) {
                        let s = &self.sums[*f as usize];
                        match &mut sum {
                            None => sum = Some(s.clone()),
                            Some(x) => x.join(s),
                        }
                        rest = rest.or(rest_at(&self.hir.funcs[*f as usize]));
                    }
                }
                match sum {
                    Some(sum) => {
                        let r = self.apply(&sum, this, args, rest);
                        (r, sum.fresh)
                    }
                    None => (self.unknown_call(this, args), false),
                }
            }
            Callee::Native(n) => (self.native(*n, this, args), false),
            Callee::Value(f) => {
                self.expr(f);
                (self.unknown_call(this, args), false)
            }
            _ => (self.unknown_call(this, args), false),
        };
        match result {
            // A fresh result is a new object, like an allocation here.
            (mut r, true) => {
                let n = self.site(e);
                self.site_call[n as usize] = true;
                r.unknown = false;
                if !r.nodes.contains(&n) {
                    r.nodes.push(n);
                }
                r
            }
            (r, false) => r,
        }
    }

    fn unknown_call(&mut self, this: Option<AV>, args: Vec<AV>) -> AV {
        if let Some(t) = &this {
            self.escape(t);
        }
        for a in &args {
            self.escape(a);
        }
        AV::unknown()
    }

    /// A call through a summary. `args[i]` is parameter `i + 1`; `this` is 0.
    fn apply(&mut self, sum: &Summary, this: Option<AV>, args: Vec<AV>, rest: Option<usize>) -> AV {
        let mut all = vec![this.unwrap_or_default()];
        all.extend(args);
        let mut result = AV::unknown();
        for (i, a) in all.iter().enumerate() {
            if rest.is_some_and(|r| i >= r) {
                // Packed into the callee's rest array (a fresh heap array).
                self.escape(a);
                continue;
            }
            let ps = sum.get(i);
            if ps.esc {
                self.escape(a);
            }
            if ps.ret {
                let cl = self.closure(a);
                result = result.union(cl);
            }
            for &t in &ps.into {
                if let Some(target) = all.get(t).cloned() {
                    let target = self.closure(&target);
                    self.store_into(&target, a);
                } else {
                    self.escape(a);
                }
            }
        }
        result
    }

    fn native(&mut self, n: Native, this: Option<AV>, args: Vec<AV>) -> AV {
        use Native::*;
        let name = format!("{n:?}");
        match n {
            // Print without retaining (inspect never runs user code).
            ConsoleLog | ConsoleError | ConsoleWarn => AV::unknown(),
            ArrPush | ArrUnshift => {
                let t = this.unwrap_or_default();
                for a in &args {
                    self.store_into(&t, a);
                }
                AV::none()
            }
            ArrPop | ArrShift | ArrAt => {
                let t = this.unwrap_or_default();
                self.read_from(&t)
            }
            ArrIndexOf | ArrLastIndexOf | ArrIncludes | ArrIsArray | ObjKeys => AV::none(),
            _ if name.starts_with("Math") || name.starts_with("Str") && !name.starts_with("Struct") || name.starts_with("Num") => {
                // Numbers and string methods: arguments are primitives.
                for a in &args {
                    self.escape(a);
                }
                AV::unknown()
            }
            _ => self.unknown_call(this, args),
        }
    }

    /// The function's summary, once its own analysis has converged.
    fn summary(&self) -> Summary {
        let hf = &self.hir.funcs[self.fid as usize];
        if hf.is_async || hf.is_generator || self.gave_up {
            return Summary::all_escape(self.n_params);
        }
        let esc = self.escaped();
        let other = self.escaped_other();
        let fresh = !self.ret_unknown
            && self.ret.iter().all(|&r| !Self::is_param(r) && !other.contains(&r));
        let mut p = vec![ParamSum::default(); self.n_params];
        for (j, ps) in p.iter_mut().enumerate() {
            let me = PARAM_BASE + j as Node;
            ps.esc = esc.contains(&me);
            ps.ret = self.ret.contains(&me);
            for (k, _) in (0..self.n_params).enumerate() {
                let tk = PARAM_BASE + k as Node;
                if k != j && self.contents.get(&tk).is_some_and(|c| c.contains(&me)) {
                    ps.into.push(k);
                }
            }
            // Stored into a site that the function returns: the caller gets it back.
            for &r in &self.ret {
                if !Self::is_param(r) && self.closure(&AV { nodes: vec![r], unknown: false }).nodes.contains(&me) {
                    ps.ret = true;
                }
            }
        }
        Summary { p, fresh }
    }

    /// Nodes that escape other than by being returned (closed over contents).
    fn escaped_other(&self) -> FxHashSet<Node> {
        let mut esc = self.esc.clone();
        for j in 0..self.n_params {
            if let Some(c) = self.contents.get(&(PARAM_BASE + j as Node)) {
                for &x in c {
                    if !Self::is_param(x) {
                        esc.insert(x);
                    }
                }
            }
        }
        self.close(&mut esc);
        esc
    }

    fn close(&self, esc: &mut FxHashSet<Node>) {
        let mut work: Vec<Node> = esc.iter().copied().collect();
        while let Some(n) = work.pop() {
            if let Some(c) = self.contents.get(&n) {
                for &x in c {
                    if esc.insert(x) {
                        work.push(x);
                    }
                }
            }
        }
    }

    /// Escaping nodes, closed over contents; returned sites and sites stored
    /// into parameters escape too.
    fn escaped(&self) -> FxHashSet<Node> {
        let mut esc = self.esc.clone();
        for &r in &self.ret {
            if !Self::is_param(r) {
                esc.insert(r);
            }
        }
        for j in 0..self.n_params {
            if let Some(c) = self.contents.get(&(PARAM_BASE + j as Node)) {
                for &x in c {
                    if !Self::is_param(x) {
                        esc.insert(x);
                    }
                }
            }
        }
        let mut work: Vec<Node> = esc.iter().copied().collect();
        while let Some(n) = work.pop() {
            if let Some(c) = self.contents.get(&n) {
                for &x in c {
                    if esc.insert(x) {
                        work.push(x);
                    }
                }
            }
        }
        esc
    }

    fn lca(&self, mut a: usize, mut b: usize) -> usize {
        let depth = |mut x: usize| {
            let mut d = 0;
            while let Some(p) = self.blocks[x].parent {
                x = p;
                d += 1;
            }
            d
        };
        let (mut da, mut db) = (depth(a), depth(b));
        while da > db {
            a = self.blocks[a].parent.unwrap();
            da -= 1;
        }
        while db > da {
            b = self.blocks[b].parent.unwrap();
            db -= 1;
        }
        while a != b {
            a = self.blocks[a].parent.unwrap();
            b = self.blocks[b].parent.unwrap();
        }
        a
    }

    /// Final decisions: each scope-bound site and its region block.
    fn decide(&self, out: &mut EscapeInfo) {
        let esc = self.escaped();
        let n = self.site_key.len();
        out.total += n;
        let mut target: Vec<usize> = self.site_block.clone();
        for (v, nodes) in &self.pts {
            let vb = self.var_block.get(v).copied().unwrap_or(0);
            for &s in nodes {
                if !Self::is_param(s) {
                    target[s as usize] = self.lca(target[s as usize], vb);
                }
            }
        }
        // A site lives at least as long as anything it is stored into.
        loop {
            let mut ch = false;
            for (&c, xs) in &self.contents {
                if Self::is_param(c) {
                    continue;
                }
                for &x in xs {
                    if Self::is_param(x) {
                        continue;
                    }
                    let t = self.lca(target[x as usize], target[c as usize]);
                    if t != target[x as usize] {
                        target[x as usize] = t;
                        ch = true;
                    }
                }
            }
            if !ch {
                break;
            }
        }
        // Only returned (and what is reachable from the returned objects):
        // allocate in the caller's result region.
        let other = self.escaped_other();
        let mut returned: FxHashSet<Node> = self.ret.iter().copied().filter(|r| !Self::is_param(*r)).collect();
        self.close(&mut returned);
        for s in 0..n {
            let sn = s as Node;
            if returned.contains(&sn) && !other.contains(&sn) && !self.site_heap[s] && !self.gave_up {
                out.result.insert(self.site_key[s]);
            }
        }
        for s in 0..n {
            if esc.contains(&(s as Node)) || self.site_heap[s] {
                continue;
            }
            let t = target[s];
            if !self.blocks[t].regionable {
                continue;
            }
            // No loop between the allocation and its region.
            let mut b = self.site_block[s];
            let mut ok = true;
            while b != t {
                if self.blocks[b].loop_body {
                    ok = false;
                    break;
                }
                b = self.blocks[b].parent.unwrap();
            }
            if ok {
                out.sites.insert(self.site_key[s], self.blocks[t].key);
                out.regions.insert(self.blocks[t].key);
            }
        }
    }

    fn run(&mut self) {
        let hf = &self.hir.funcs[self.fid as usize];
        // Iterate to a fixpoint (variables may be assigned after they're read).
        for _ in 0..16 {
            self.changed = false;
            self.cur = vec![0];
            for p in &hf.params {
                if let Some(d) = &p.default {
                    let av = self.expr(d);
                    self.add_pts(p.var, &av);
                }
            }
            for s in &hf.body {
                self.stmt(s);
            }
            if !self.changed {
                return;
            }
        }
        // No fixpoint: give up on this function.
        self.gave_up = true;
        let all: Vec<Node> = (0..self.site_key.len() as Node).collect();
        self.esc.extend(all);
    }
}

/// The argument index (0 = `this`) from which arguments are packed into the
/// callee's rest array.
fn rest_at(hf: &HFunc) -> Option<usize> {
    hf.params.iter().position(|p| p.rest).map(|i| i + 1)
}

fn is_subclass(hir: &HProgram, mut k: u32, of: u32) -> bool {
    loop {
        if k == of {
            return true;
        }
        match hir.classes[k as usize].parent {
            Some(p) => k = p,
            None => return false,
        }
    }
}

/// Analyze the whole program.
pub fn analyze(hir: &HProgram) -> EscapeInfo {
    let n = hir.funcs.len();
    let mut sums: Vec<Summary> = hir
        .funcs
        .iter()
        .map(|f| if f.is_async || f.is_generator { Summary::all_escape(f.params.len() + 1) } else { Summary { p: vec![ParamSum::default(); f.params.len() + 1], fresh: true } })
        .collect();
    let mut converged = false;
    for _round in 0..20 {
        let mut changed = false;
        for fid in 0..n {
            let mut fa = Fa::new(hir, fid as FuncId, &sums);
            fa.run();
            let s = fa.summary();
            if s != sums[fid] {
                sums[fid] = s;
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        // Optimistic summaries that never settled are not safe to use.
        for (s, f) in sums.iter_mut().zip(&hir.funcs) {
            *s = Summary::all_escape(f.params.len() + 1);
        }
    }
    let mut out = EscapeInfo::default();
    for fid in 0..n {
        let hf = &hir.funcs[fid];
        if hf.is_async || hf.is_generator {
            continue;
        }
        let mut fa = Fa::new(hir, fid as FuncId, &sums);
        fa.run();
        fa.decide(&mut out);
    }
    out
}
