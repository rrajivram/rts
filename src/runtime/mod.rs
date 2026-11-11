//! Run-time values. All heap values are reference counted (`Rc`); there is no
//! tracing garbage collector. Numbers, booleans and BigInts in typed code never
//! reach this module — they live unboxed in the VM's number and boolean slots.

pub mod bigint;
pub mod natives;
pub mod task;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};
use rustc_hash::{FxBuildHasher, FxHashMap};

pub use natives::Native;

/// A JavaScript string. `ascii` lets length and indexing use byte offsets;
/// other strings fall back to UTF-16 semantics computed on the fly.
#[derive(Debug)]
pub struct JsStr {
    pub s: Box<str>,
    pub ascii: bool,
}

impl JsStr {
    pub fn new(s: impl Into<Box<str>>) -> Rc<JsStr> {
        let s = s.into();
        let ascii = s.is_ascii();
        Rc::new(JsStr { s, ascii })
    }
    pub fn len16(&self) -> usize {
        if self.ascii {
            self.s.len()
        } else {
            self.s.encode_utf16().count()
        }
    }
    pub fn utf16(&self) -> Vec<u16> {
        self.s.encode_utf16().collect()
    }
    pub fn code_unit(&self, i: usize) -> Option<u16> {
        if self.ascii {
            self.s.as_bytes().get(i).map(|b| *b as u16)
        } else {
            self.s.encode_utf16().nth(i)
        }
    }
    /// Substring by UTF-16 indices (clamped).
    pub fn slice16(&self, start: usize, end: usize) -> String {
        if self.ascii {
            let end = end.min(self.s.len());
            let start = start.min(end);
            self.s[start..end].to_string()
        } else {
            let u = self.utf16();
            let end = end.min(u.len());
            let start = start.min(end);
            String::from_utf16_lossy(&u[start..end])
        }
    }
}

#[derive(Debug)]
pub enum ArrStore {
    Num(Vec<f64>),
    Val(Vec<Value>),
}

#[derive(Debug)]
pub struct Array {
    pub store: RefCell<ArrStore>,
}

impl Array {
    pub fn new_num(v: Vec<f64>) -> Rc<Array> {
        Rc::new(Array {
            store: RefCell::new(ArrStore::Num(v)),
        })
    }
    pub fn new_val(v: Vec<Value>) -> Rc<Array> {
        Rc::new(Array {
            store: RefCell::new(ArrStore::Val(v)),
        })
    }
    pub fn len(&self) -> usize {
        match &*self.store.borrow() {
            ArrStore::Num(v) => v.len(),
            ArrStore::Val(v) => v.len(),
        }
    }
    pub fn get(&self, i: usize) -> Value {
        match &*self.store.borrow() {
            ArrStore::Num(v) => v.get(i).map_or(Value::Undef, |n| Value::Num(*n)),
            ArrStore::Val(v) => v.get(i).cloned().unwrap_or(Value::Undef),
        }
    }
    pub fn to_vec(&self) -> Vec<Value> {
        match &*self.store.borrow() {
            ArrStore::Num(v) => v.iter().map(|n| Value::Num(*n)).collect(),
            ArrStore::Val(v) => v.clone(),
        }
    }
    /// Write a value, degrading the storage to `Val` when a non-number is
    /// written into a numeric array (possible through `any` or covariance).
    pub fn set(&self, i: usize, val: Value) {
        let mut st = self.store.borrow_mut();
        if let ArrStore::Num(v) = &mut *st {
            if let Value::Num(n) = val {
                if i < v.len() {
                    v[i] = n;
                } else if i == v.len() {
                    v.push(n);
                } else {
                    // Holes: degrade (holes read as undefined).
                    let mut vals: Vec<Value> = v.iter().map(|n| Value::Num(*n)).collect();
                    vals.resize(i, Value::Undef);
                    vals.push(Value::Num(n));
                    *st = ArrStore::Val(vals);
                }
                return;
            }
            let vals: Vec<Value> = v.iter().map(|n| Value::Num(*n)).collect();
            *st = ArrStore::Val(vals);
        }
        let ArrStore::Val(v) = &mut *st else { unreachable!() };
        if i >= v.len() {
            v.resize(i + 1, Value::Undef);
        }
        v[i] = val;
    }
    pub fn push(&self, val: Value) {
        let len = self.len();
        self.set(len, val);
    }
    pub fn set_len(&self, n: usize) {
        match &mut *self.store.borrow_mut() {
            ArrStore::Num(v) => {
                if n <= v.len() {
                    v.truncate(n)
                } else {
                    v.resize(n, f64::NAN)
                }
            }
            ArrStore::Val(v) => v.resize(n, Value::Undef),
        }
    }
}

/// An ordered list of property names. Objects built the same way share a
/// shape, so inline caches can key on the shape pointer.
#[derive(Debug, Default)]
pub struct Shape {
    pub keys: Vec<Rc<str>>,
    map: FxHashMap<Rc<str>, u32>,
    transitions: RefCell<FxHashMap<Rc<str>, Rc<Shape>>>,
}

impl Shape {
    pub fn new(keys: Vec<Rc<str>>) -> Rc<Shape> {
        let map = keys.iter().enumerate().map(|(i, k)| (k.clone(), i as u32)).collect();
        Rc::new(Shape {
            keys,
            map,
            transitions: Default::default(),
        })
    }
    pub fn lookup(&self, name: &str) -> Option<u32> {
        if self.keys.len() <= 8 {
            self.keys.iter().position(|k| &**k == name).map(|i| i as u32)
        } else {
            self.map.get(name).copied()
        }
    }
    /// Shape with one more key, shared through a transition table.
    pub fn with_key(self: &Rc<Shape>, name: &Rc<str>) -> Rc<Shape> {
        if let Some(s) = self.transitions.borrow().get(name) {
            return s.clone();
        }
        let mut keys = self.keys.clone();
        keys.push(name.clone());
        let s = Shape::new(keys);
        // Big (dictionary-like) objects don't cache transitions.
        if self.keys.len() < 64 {
            self.transitions.borrow_mut().insert(name.clone(), s.clone());
        }
        s
    }
    pub fn without_key(&self, idx: usize) -> Rc<Shape> {
        let mut keys = self.keys.clone();
        keys.remove(idx);
        Shape::new(keys)
    }
}

/// Fields stored inline for small objects (one allocation per object).
pub type Fields = smallvec::SmallVec<[Value; 4]>;

#[derive(Debug)]
pub struct ObjInner {
    pub shape: Rc<Shape>,
    pub vals: Fields,
}

#[derive(Debug)]
pub struct Object {
    pub inner: RefCell<ObjInner>,
    pub class: Option<Rc<ClassRt>>,
}

impl Object {
    pub fn new(shape: Rc<Shape>, vals: Vec<Value>, class: Option<Rc<ClassRt>>) -> Rc<Object> {
        Self::with_fields(shape, Fields::from_vec(vals), class)
    }
    /// An object with `n` undefined fields.
    pub fn blank(shape: Rc<Shape>, class: Option<Rc<ClassRt>>) -> Rc<Object> {
        let n = shape.keys.len();
        Self::with_fields(shape, Fields::from_elem(Value::Undef, n), class)
    }
    pub fn with_fields(shape: Rc<Shape>, vals: Fields, class: Option<Rc<ClassRt>>) -> Rc<Object> {
        Rc::new(Object {
            inner: RefCell::new(ObjInner { shape, vals }),
            class,
        })
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        let inner = self.inner.borrow();
        inner.shape.lookup(name).map(|i| inner.vals[i as usize].clone())
    }
    pub fn set(&self, name: &Rc<str>, val: Value) {
        let mut inner = self.inner.borrow_mut();
        if let Some(i) = inner.shape.lookup(name) {
            inner.vals[i as usize] = val;
        } else {
            inner.shape = inner.shape.with_key(name);
            inner.vals.push(val);
        }
    }
    pub fn delete(&self, name: &str) -> bool {
        let mut inner = self.inner.borrow_mut();
        if let Some(i) = inner.shape.lookup(name) {
            inner.shape = inner.shape.without_key(i as usize);
            inner.vals.remove(i as usize);
        }
        true
    }
    pub fn keys(&self) -> Vec<Rc<str>> {
        self.inner.borrow().shape.keys.clone()
    }
}

/// Run-time class: instance shape, vtable and constructor.
#[derive(Debug)]
pub struct ClassRt {
    pub id: u32,
    pub name: Rc<str>,
    pub parent: Option<Rc<ClassRt>>,
    pub shape: Rc<Shape>,
    pub vtable: Vec<u32>,
    pub methods: FxHashMap<Rc<str>, u32>,
    pub ctor: u32,
}

impl ClassRt {
    pub fn is_subclass_of(&self, id: u32) -> bool {
        let mut c = Some(self);
        while let Some(k) = c {
            if k.id == id {
                return true;
            }
            c = k.parent.as_deref();
        }
        false
    }
}

#[derive(Debug)]
pub struct Closure {
    pub func: u32,
    pub caps: Box<[Rc<RefCell<Value>>]>,
}

/// Hash key with SameValueZero semantics (for Map and Set).
#[derive(Debug, Clone)]
pub struct HKey(pub Value);

impl PartialEq for HKey {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Value::Num(a), Value::Num(b)) => a == b || (a.is_nan() && b.is_nan()),
            (a, b) => a.strict_eq(b),
        }
    }
}
impl Eq for HKey {}
impl Hash for HKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match &self.0 {
            Value::Num(n) => {
                let n = if *n == 0.0 { 0.0 } else { *n };
                let bits = if n.is_nan() { f64::NAN.to_bits() } else { n.to_bits() };
                0u8.hash(h);
                bits.hash(h);
            }
            Value::Str(s) => {
                1u8.hash(h);
                s.s.hash(h);
            }
            Value::Bool(b) => b.hash(h),
            Value::BigInt(n) => {
                4u8.hash(h);
                n.hash(h);
            }
            Value::Undef => 2u8.hash(h),
            Value::Null => 3u8.hash(h),
            v => v.ptr_id().hash(h),
        }
    }
}

pub type MapObj = RefCell<IndexMap<HKey, Value, FxBuildHasher>>;
pub type SetObj = RefCell<IndexSet<HKey, FxBuildHasher>>;

#[derive(Debug, Clone, Default)]
pub enum Value {
    #[default]
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    /// Boxed so the rare BigInt doesn't make every value 16-byte aligned.
    BigInt(Rc<i128>),
    Str(Rc<JsStr>),
    Arr(Rc<Array>),
    Obj(Rc<Object>),
    Func(Rc<Closure>),
    /// A captured variable's box (internal; never visible to programs).
    Cell(Rc<RefCell<Value>>),
    Class(Rc<ClassRt>),
    Map(Rc<MapObj>),
    Set(Rc<SetObj>),
    /// A built-in function used as a value.
    Native(Native),
    /// A bound function (`f.bind(…)`, a promise's resolve/reject functions).
    Bound(Rc<task::Bound>),
    Symbol(Rc<task::Symbol>),
    /// A generator object (also the state of an async function, internally).
    Gen(Rc<task::Coroutine>),
    Promise(Rc<task::Promise>),
    /// A built-in iterator (`arr.values()`, `map.entries()`, …).
    Iter(Rc<task::NativeIter>),
}

const _: () = assert!(std::mem::size_of::<Value>() == 16);

impl Value {
    /// Drop a slot's heap reference, if it holds one. Slots holding
    /// primitives keep their stale contents: nothing to free, nothing to do.
    #[inline(always)]
    pub fn release(&mut self) {
        if !matches!(self, Value::Undef | Value::Null | Value::Bool(_) | Value::Num(_) | Value::Native(_)) {
            *self = Value::Undef;
        }
    }
}

impl Value {
    pub fn str(s: impl Into<Box<str>>) -> Value {
        Value::Str(JsStr::new(s))
    }

    pub fn bigint(n: i128) -> Value {
        Value::BigInt(Rc::new(n))
    }

    pub fn ptr_id(&self) -> usize {
        match self {
            Value::Str(s) => Rc::as_ptr(s) as *const () as usize,
            Value::Arr(a) => Rc::as_ptr(a) as *const () as usize,
            Value::Obj(o) => Rc::as_ptr(o) as *const () as usize,
            Value::Func(f) => Rc::as_ptr(f) as *const () as usize,
            Value::Cell(c) => Rc::as_ptr(c) as *const () as usize,
            Value::Class(c) => Rc::as_ptr(c) as *const () as usize,
            Value::Map(m) => Rc::as_ptr(m) as *const () as usize,
            Value::Set(s) => Rc::as_ptr(s) as *const () as usize,
            Value::Native(n) => *n as usize,
            Value::Bound(b) => Rc::as_ptr(b) as *const () as usize,
            Value::Symbol(x) => Rc::as_ptr(x) as *const () as usize,
            Value::Gen(g) => Rc::as_ptr(g) as *const () as usize,
            Value::Promise(p) => Rc::as_ptr(p) as *const () as usize,
            Value::Iter(i) => Rc::as_ptr(i) as *const () as usize,
            _ => 0,
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Undef | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::BigInt(n) => **n != 0,
            Value::Str(s) => !s.s.is_empty(),
            _ => true,
        }
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undef | Value::Null)
    }

    pub fn strict_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Undef, Value::Undef) | (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => Rc::ptr_eq(a, b) || a.s == b.s,
            (Value::Native(a), Value::Native(b)) => a == b,
            (a, b) => {
                std::mem::discriminant(a) == std::mem::discriminant(b) && a.ptr_id() == b.ptr_id() && a.ptr_id() != 0
            }
        }
    }

    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (a, b) if a.is_nullish() && b.is_nullish() => true,
            (a, b) if a.is_nullish() || b.is_nullish() => false,
            (Value::BigInt(a), b) | (b, Value::BigInt(a)) if !matches!(b, Value::BigInt(_)) => match b {
                Value::Str(s) => bigint::parse(&s.s) == Some(**a),
                Value::Obj(_) | Value::Arr(_) => bigint::parse(&b.to_display()) == Some(**a),
                b => bigint::cmp_number(**a, b.to_number()).is_some_and(|o| o.is_eq()),
            },
            (Value::Num(_), Value::Str(_)) | (Value::Str(_), Value::Num(_)) | (Value::Bool(_), _) | (_, Value::Bool(_))
                if !matches!((self, other), (Value::Bool(_), Value::Bool(_))) =>
            {
                let (a, b) = (self.to_number(), other.to_number());
                a == b
            }
            (a, b) => a.strict_eq(b),
        }
    }

    /// ToNumber, except that a BigInt converts to the nearest Number (as
    /// `Number(x)` does) instead of throwing; operators check for BigInts first.
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::BigInt(n) => **n as f64,
            Value::Bool(b) => *b as u8 as f64,
            Value::Undef => f64::NAN,
            Value::Null => 0.0,
            Value::Str(s) => str_to_number(&s.s),
            Value::Arr(a) => match a.len() {
                0 => 0.0,
                1 => a.get(0).to_number(),
                _ => f64::NAN,
            },
            _ => f64::NAN,
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undef => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::BigInt(_) => "bigint",
            Value::Str(_) => "string",
            Value::Func(_) | Value::Class(_) | Value::Native(_) | Value::Bound(_) => "function",
            Value::Symbol(_) => "symbol",
            _ => "object",
        }
    }

    /// ToString, as used by `String(x)`, `+` and template literals.
    pub fn to_js_string(&self) -> Rc<JsStr> {
        match self {
            Value::Str(s) => s.clone(),
            // As a property key: the symbol's hidden key string.
            Value::Symbol(s) => JsStr::new(&*s.key),
            v => JsStr::new(v.to_display()),
        }
    }

    pub fn to_display(&self) -> String {
        match self {
            Value::Undef => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => num_to_string(*n),
            Value::BigInt(n) => n.to_string(),
            Value::Str(s) => s.s.to_string(),
            Value::Arr(a) => a
                .to_vec()
                .iter()
                .map(|v| if v.is_nullish() { String::new() } else { v.to_display() })
                .collect::<Vec<_>>()
                .join(","),
            Value::Obj(o) => {
                // Error-like objects print as "Name: message".
                if let (Some(n), Some(m)) = (o.get("name"), o.get("message"))
                    && o.class.is_some()
                {
                    let m = m.to_display();
                    if m.is_empty() {
                        return n.to_display();
                    }
                    return format!("{}: {}", n.to_display(), m);
                }
                "[object Object]".into()
            }
            Value::Func(_) | Value::Native(_) | Value::Bound(_) => "function () { [native code] }".into(),
            Value::Symbol(s) => format!("Symbol({})", s.description.as_deref().unwrap_or("")),
            Value::Gen(_) => "[object Generator]".into(),
            Value::Promise(_) => "[object Promise]".into(),
            Value::Iter(i) => format!("[object {}]", i.kind),
            Value::Class(c) => format!("class {} {{ }}", c.name),
            Value::Map(_) => "[object Map]".into(),
            Value::Set(_) => "[object Set]".into(),
            Value::Cell(c) => c.borrow().to_display(),
        }
    }
}

impl Value {
    fn to_display_with(self, f: impl FnOnce(&Value) -> String) -> String {
        f(&self)
    }
}

/// How a symbol key prints: `Symbol(Symbol.iterator)`, `Symbol(desc)`.
pub fn symbol_key_display(key_body: &str) -> String {
    // User symbols are "<id>:<description>"; well-known ones "Symbol.x".
    let desc = match key_body.split_once(':') {
        Some((id, d)) if id.chars().all(|c| c.is_ascii_digit()) => d,
        _ => key_body,
    };
    format!("Symbol({desc})")
}

pub fn str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map_or(f64::NAN, |v| v as f64);
    }
    if let Some(h) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return u64::from_str_radix(h, 2).map_or(f64::NAN, |v| v as f64);
    }
    if let Some(h) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return u64::from_str_radix(h, 8).map_or(f64::NAN, |v| v as f64);
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust accepts "inf"/"nan"; JS doesn't.
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// Number::toString(10) as specified by ECMAScript.
pub fn num_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    // Shortest round-trip digits and exponent.
    let e = format!("{:e}", x.abs());
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{}", if n - 1 >= 0 { "+" } else { "-" }, (n - 1).abs());
    }
    out
}

// ───────────────────────────── console.log formatting ─────────────────────────────

/// A port of the parts of Node's `util.inspect` that console.log uses
/// (breakLength 80, compact 3, depth 2).
pub fn inspect(v: &Value, funcs: &dyn Fn(u32) -> Rc<str>) -> String {
    let mut cx = Insp {
        indent: 0,
        cur_depth: 0,
        seen: Vec::new(),
        funcs,
    };
    match v {
        Value::Str(s) => s.s.to_string(),
        v => cx.value(v, 0),
    }
}

struct Insp<'a> {
    indent: usize,
    cur_depth: usize,
    seen: Vec<usize>,
    funcs: &'a dyn Fn(u32) -> Rc<str>,
}

fn quote_str(s: &str) -> String {
    let q = if !s.contains('\'') {
        '\''
    } else if !s.contains('"') {
        '"'
    } else if !s.contains('`') {
        '`'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            c if c == q => {
                out.push('\\');
                out.push(c)
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn key_str(k: &str) -> String {
    if let Some(name) = k.strip_prefix(task::SYMBOL_PREFIX) {
        return format!("[{}]", symbol_key_display(name));
    }
    let ident = !k.is_empty()
        && !k.chars().next().unwrap().is_ascii_digit()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ident { k.to_string() } else { quote_str(k) }
}

impl Insp<'_> {
    fn value(&mut self, v: &Value, depth: usize) -> String {
        match v {
            Value::Str(s) => quote_str(&s.s),
            Value::Num(n) if *n == 0.0 && n.is_sign_negative() => "-0".into(),
            Value::BigInt(n) => format!("{n}n"),
            Value::Func(f) => {
                let name = (self.funcs)(f.func);
                if name.is_empty() {
                    "[Function (anonymous)]".into()
                } else {
                    format!("[Function: {name}]")
                }
            }
            Value::Native(n) => format!("[Function: {}]", n.name()),
            Value::Bound(b) => match &b.target {
                Value::Func(f) => format!("[Function: bound {}]", (self.funcs)(f.func)),
                Value::Native(n) if !n.name().is_empty() => format!("[Function: bound {}]", n.name()),
                _ => "[Function (anonymous)]".into(),
            },
            Value::Class(c) => format!("[class {}]", c.name),
            Value::Symbol(s) => format!("Symbol({})", s.description.as_deref().unwrap_or("")),
            Value::Iter(i) => format!("Object [{}] {{}}", i.kind),
            Value::Gen(g) => match g.kind {
                task::CoKind::AsyncGenerator => "Object [AsyncGenerator] {}".into(),
                _ => "Object [Generator] {}".into(),
            },
            Value::Promise(p) => {
                let inner = match &*p.state.borrow() {
                    task::PromiseState::Pending(_) => "<pending>".to_string(),
                    task::PromiseState::Fulfilled(v) => v.clone().to_display_with(|v| self.value(v, depth + 1)),
                    task::PromiseState::Rejected(v) => format!("<rejected> {}", v.clone().to_display_with(|v| self.value(v, depth + 1))),
                };
                format!("Promise {{ {inner} }}")
            }
            Value::Arr(_) | Value::Obj(_) | Value::Map(_) | Value::Set(_) => self.compound(v, depth),
            v => v.to_display(),
        }
    }

    fn compound(&mut self, v: &Value, depth: usize) -> String {
        if let Value::Obj(o) = v
            && let Some(c) = &o.class
            && is_error_class(c)
        {
            let s = v.to_display();
            return if depth == 0 { s } else { format!("[{s}]") };
        }
        let id = v.ptr_id();
        if self.seen.contains(&id) {
            return "[Circular *1]".into();
        }
        if depth > 2 {
            return match v {
                Value::Arr(_) => "[Array]".into(),
                Value::Obj(o) => match &o.class {
                    Some(c) => format!("[{}]", c.name),
                    None => "[Object]".into(),
                },
                Value::Map(_) => "[Map]".into(),
                _ => "[Set]".into(),
            };
        }
        self.seen.push(id);
        self.cur_depth = depth;
        let (base, braces, output, is_array, numeric) = match v {
            Value::Arr(a) => {
                let vals = a.to_vec();
                self.indent += 2;
                let mut out: Vec<String> = vals.iter().take(100).map(|x| self.value(x, depth + 1)).collect();
                self.indent -= 2;
                if vals.len() > 100 {
                    out.push(format!("... {} more item{}", vals.len() - 100, if vals.len() - 100 > 1 { "s" } else { "" }));
                }
                let numeric = vals.iter().all(|x| matches!(x, Value::Num(_)));
                (String::new(), ("[", "]"), out, true, numeric)
            }
            Value::Obj(o) => {
                let (keys, vals) = {
                    let inner = o.inner.borrow();
                    (inner.shape.keys.clone(), inner.vals.clone())
                };
                self.indent += 2;
                let out: Vec<String> = keys
                    .iter()
                    .zip(vals.iter())
                    .map(|(k, x)| format!("{}: {}", key_str(k), self.value(x, depth + 1)))
                    .collect();
                self.indent -= 2;
                let base = match &o.class {
                    Some(c) => c.name.to_string(),
                    None => String::new(),
                };
                (base, ("{", "}"), out, false, false)
            }
            Value::Map(m) => {
                let entries: Vec<(Value, Value)> = m.borrow().iter().map(|(k, x)| (k.0.clone(), x.clone())).collect();
                self.indent += 2;
                let out: Vec<String> = entries
                    .iter()
                    .map(|(k, x)| format!("{} => {}", self.value(k, depth + 1), self.value(x, depth + 1)))
                    .collect();
                self.indent -= 2;
                (format!("Map({})", entries.len()), ("{", "}"), out, false, false)
            }
            Value::Set(s) => {
                let items: Vec<Value> = s.borrow().iter().map(|k| k.0.clone()).collect();
                self.indent += 2;
                let out: Vec<String> = items.iter().map(|x| self.value(x, depth + 1)).collect();
                self.indent -= 2;
                (format!("Set({})", items.len()), ("{", "}"), out, true, false)
            }
            _ => unreachable!(),
        };
        self.seen.pop();
        let res = self.reduce(output, &base, braces, is_array, numeric, depth);
        res
    }

    fn reduce(&mut self, mut output: Vec<String>, base: &str, braces: (&str, &str), is_array: bool, numeric: bool, depth: usize) -> String {
        if output.is_empty() {
            return if base.is_empty() {
                format!("{}{}", braces.0, braces.1)
            } else {
                format!("{base} {}{}", braces.0, braces.1)
            };
        }
        let entries = output.len();
        if is_array && entries > 6 {
            output = self.group(output, numeric);
        }
        if self.cur_depth - depth < 3 && entries == output.len() {
            let start = output.len() + self.indent + braces.0.len() + base.len() + 10;
            let total: usize = output.iter().map(|s| s.chars().count()).sum::<usize>() + output.len() + start;
            if output.len() * 2 + start <= 80 && total <= 80 {
                let joined = output.join(", ");
                if !joined.contains('\n') {
                    let b = if base.is_empty() { String::new() } else { format!("{base} ") };
                    return format!("{b}{} {joined} {}", braces.0, braces.1);
                }
            }
        }
        let ind = format!("\n{}", " ".repeat(self.indent));
        let b = if base.is_empty() { String::new() } else { format!("{base} ") };
        format!("{b}{}{ind}  {}{ind}{}", braces.0, output.join(&format!(",{ind}  ")), braces.1)
    }

    fn group(&self, output: Vec<String>, numeric: bool) -> Vec<String> {
        let mut total = 0usize;
        let mut max_len = 0usize;
        let mut output_len = output.len();
        if output.last().is_some_and(|s| s.starts_with("... ")) {
            output_len -= 1;
        }
        let data_len: Vec<usize> = output.iter().map(|s| s.chars().count()).collect();
        for &len in data_len.iter().take(output_len) {
            total += len + 2;
            max_len = max_len.max(len);
        }
        let actual_max = max_len + 2;
        if actual_max * 3 + self.indent < 80 && (total as f64 / actual_max as f64 > 5.0 || max_len <= 6) {
            let average_bias = (actual_max as f64 - total as f64 / output.len() as f64).sqrt();
            let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
            let columns = ((2.5 * biased_max * output_len as f64).sqrt() / biased_max)
                .round()
                .min(((80 - self.indent) / actual_max) as f64)
                .min(12.0)
                .min(15.0) as usize;
            if columns <= 1 {
                return output;
            }
            let mut max_line = Vec::new();
            for i in 0..columns {
                let mut line_len = 0;
                let mut j = i;
                while j < output.len() {
                    line_len = line_len.max(data_len[j]);
                    j += columns;
                }
                max_line.push(line_len + 2);
            }
            let mut tmp = Vec::new();
            let mut i = 0;
            while i < output_len {
                let max = (i + columns).min(output_len);
                let mut s = String::new();
                let mut j = i;
                while j < max - 1 {
                    let cell = format!("{}, ", output[j]);
                    let w = max_line[j - i];
                    if numeric {
                        s.push_str(&format!("{cell:>w$}"));
                    } else {
                        s.push_str(&format!("{cell:<w$}"));
                    }
                    j += 1;
                }
                if numeric {
                    let w = max_line[j - i] - 2;
                    s.push_str(&format!("{:>w$}", output[j]));
                } else {
                    s.push_str(&output[j]);
                }
                tmp.push(s);
                i += columns;
            }
            if output_len < output.len() {
                tmp.push(output[output_len].clone());
            }
            return tmp;
        }
        output
    }
}

fn is_error_class(c: &ClassRt) -> bool {
    let mut k = Some(c);
    while let Some(c) = k {
        if &*c.name == "Error" {
            return true;
        }
        k = c.parent.as_deref();
    }
    false
}

/// Per-instruction inline cache for property lookups by name. Holds the
/// shape strongly so a freed shape's address can never produce a false hit.
#[derive(Debug, Default)]
pub struct PropCache {
    pub entry: RefCell<Option<(Rc<Shape>, u32)>>,
    /// Method-call sites: (receiver shape, receiver class) → function index,
    /// for a name that resolves to a class method rather than an own field.
    pub method: RefCell<Option<(Rc<Shape>, Rc<ClassRt>, u32)>>,
}
