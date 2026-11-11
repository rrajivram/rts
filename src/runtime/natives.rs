//! Built-in functions and methods. Natives take boxed `Value`s; hot built-ins
//! on typed receivers (`Math.*`, `arr.push` on `number[]`, `s.charCodeAt`)
//! are compiled to dedicated instructions instead and never come here.

use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use super::*;
use crate::vm::Vm;

pub type NResult = Result<Value, Value>;

macro_rules! natives {
    ($($v:ident = $name:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u16)]
        pub enum Native { $($v),* }
        impl Native {
            pub fn name(self) -> &'static str {
                match self { $(Native::$v => $name),* }
            }
        }
    };
}

natives! {
    ConsoleLog = "log", ConsoleError = "error", ConsoleWarn = "warn",
    MathFloor = "floor", MathCeil = "ceil", MathRound = "round", MathTrunc = "trunc", MathSqrt = "sqrt",
    MathAbs = "abs", MathSign = "sign", MathSin = "sin", MathCos = "cos", MathTan = "tan", MathAsin = "asin",
    MathAcos = "acos", MathAtan = "atan", MathAtan2 = "atan2", MathExp = "exp", MathLog = "log",
    MathLog2 = "log2", MathLog10 = "log10", MathPow = "pow", MathMin = "min", MathMax = "max",
    MathRandom = "random", MathHypot = "hypot", MathCbrt = "cbrt", MathImul = "imul", MathClz32 = "clz32",
    MathFround = "fround",
    ParseInt = "parseInt", ParseFloat = "parseFloat", IsNaN = "isNaN", IsFinite = "isFinite",
    StringFn = "String", NumberFn = "Number", BooleanFn = "Boolean", BigIntFn = "BigInt",
    NumIsInteger = "isInteger", NumIsFinite = "isFinite", NumIsNaN = "isNaN", NumIsSafeInteger = "isSafeInteger",
    NumToFixed = "toFixed", NumToString = "toString", NumToPrecision = "toPrecision",
    StrCharAt = "charAt", StrCharCodeAt = "charCodeAt", StrCodePointAt = "codePointAt", StrIndexOf = "indexOf",
    StrLastIndexOf = "lastIndexOf", StrIncludes = "includes", StrStartsWith = "startsWith",
    StrEndsWith = "endsWith", StrSlice = "slice", StrSubstring = "substring", StrSubstr = "substr",
    StrToUpper = "toUpperCase", StrToLower = "toLowerCase", StrTrim = "trim", StrTrimStart = "trimStart",
    StrTrimEnd = "trimEnd", StrSplit = "split", StrRepeat = "repeat", StrPadStart = "padStart",
    StrPadEnd = "padEnd", StrReplace = "replace", StrReplaceAll = "replaceAll", StrConcat = "concat",
    StrAt = "at", StrLocaleCompare = "localeCompare", StrFromCharCode = "fromCharCode",
    StrToString = "toString", StrNormalize = "normalize",
    ArrPush = "push", ArrPop = "pop", ArrShift = "shift", ArrUnshift = "unshift", ArrSlice = "slice",
    ArrSplice = "splice", ArrConcat = "concat", ArrJoin = "join", ArrReverse = "reverse",
    ArrIndexOf = "indexOf", ArrLastIndexOf = "lastIndexOf", ArrIncludes = "includes", ArrMap = "map",
    ArrFilter = "filter", ArrForEach = "forEach", ArrReduce = "reduce", ArrReduceRight = "reduceRight",
    ArrFind = "find", ArrFindIndex = "findIndex", ArrFindLast = "findLast", ArrFindLastIndex = "findLastIndex",
    ArrSome = "some", ArrEvery = "every", ArrSort = "sort", ArrFill = "fill", ArrAt = "at", ArrFlat = "flat",
    ArrFlatMap = "flatMap", ArrIsArray = "isArray", ArrFrom = "from", ArrOf = "of", ArrNew = "Array",
    ArrToString = "toString", ArrKeys = "keys", ArrEntries = "entries",
    ObjKeys = "keys", ObjValues = "values", ObjEntries = "entries", ObjAssign = "assign",
    ObjFreeze = "freeze", ObjFromEntries = "fromEntries",
    JsonStringify = "stringify", JsonParse = "parse",
    MapNew = "Map", MapGet = "get", MapSet = "set", MapHas = "has", MapDelete = "delete", MapClear = "clear",
    MapSize = "size", MapForEach = "forEach", MapKeys = "keys", MapValues = "values", MapEntries = "entries",
    SetNew = "Set", SetAdd = "add", SetHas = "has", SetDelete = "delete", SetClear = "clear", SetSize = "size",
    SetForEach = "forEach", SetValues = "values",
    DateNow = "now", PerfNow = "now",
    GenNext = "next", GenReturn = "return", GenThrow = "throw", IterSelf = "\u{2}Symbol.iterator",
    SymbolFn = "Symbol", SymbolFor = "for",
    PromiseNew = "Promise", PromiseResolveFunction = "", PromiseRejectFunction = "", PromiseThunk = "",
    PromiseThen = "then", PromiseCatch = "catch", PromiseFinally = "finally", PromiseResolve = "resolve",
    PromiseReject = "reject", PromiseAll = "all", PromiseAllSettled = "allSettled", PromiseAny = "any",
    PromiseRace = "race", PromiseWithResolvers = "withResolvers",
    SetTimeout = "setTimeout", SetInterval = "setInterval", SetImmediate = "setImmediate",
    ClearTimeout = "clearTimeout", QueueMicrotask = "queueMicrotask", NextTick = "nextTick",
    FunctionBind = "bind", FunctionCall = "call", FunctionApply = "apply",
    SymbolWellKnown = "", IsInstanceOfBuiltin = "",
    RegexNew = "RegExp", RegexTest = "test", RegexExec = "exec", StrMatch = "match", StrMatchAll = "matchAll",
    StrSearch = "search", ObjRest = "", ObjDefineAccessor = "", SymbolToString = "toString",
    TemplateObject = "", StructuredClone = "structuredClone", ArrToSorted = "toSorted",
    ArrToReversed = "toReversed", ArrWith = "with", GlobalThis = "",
    // Node (src/node/natives.rs)
    ProcessObject = "", ProcessExit = "exit", ProcessCwd = "cwd", ProcessChdir = "chdir", StdoutWrite = "write",
    StderrWrite = "write", HrtimeBigint = "bigint", FsReadFile = "readFileSync", FsWriteFile = "writeFileSync",
    FsAppendFile = "appendFileSync", FsExists = "existsSync", FsMkdir = "mkdirSync", FsReaddir = "readdirSync",
    FsStat = "statSync", FsUnlink = "unlinkSync", FsRm = "rmSync", FsRename = "renameSync", FsCopy = "copyFileSync",
    FsRealpath = "realpathSync", OsInfo = "", UtilFormat = "format", UtilInspect = "inspect", EnvObject = "",
    DateNowMs = "now", DateOffset = "", DateZoneName = "", DateParse = "parse", DeepEqual = "isDeepStrictEqual",
    ObjectIs = "is", EncodeUriComponent = "encodeURIComponent", EncodeUri = "encodeURI",
    DecodeUriComponent = "decodeURIComponent", DecodeUri = "decodeURI", NoOp = "",
    ConsoleDir = "dir", ConsoleGroup = "group", ConsoleGroupEnd = "groupEnd", ConsoleAssert = "assert",
    ConsoleCount = "count", ConsoleCountReset = "countReset", ConsoleTime = "time", ConsoleTimeEnd = "timeEnd",
    ConsoleTable = "table", ObjDefineProperty = "defineProperty", ObjCreate = "create",
    ObjGetPrototypeOf = "getPrototypeOf", ObjSetPrototypeOf = "setPrototypeOf", ObjIsFrozen = "isFrozen",
    ObjGroupBy = "groupBy", ObjHasOwn = "hasOwn", StrRaw = "raw",
}

impl Native {
    /// Implemented in `vm_task.rs` (generators, promises, timers, functions).
    pub fn is_task(self) -> bool {
        (Native::GenNext as u16..=Native::IsInstanceOfBuiltin as u16).contains(&(self as u16))
    }
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undef)
}

fn num(args: &[Value], i: usize) -> f64 {
    args.get(i).map_or(f64::NAN, |v| v.to_number())
}

fn int_arg(args: &[Value], i: usize, default: f64) -> f64 {
    match args.get(i) {
        None | Some(Value::Undef) => default,
        Some(v) => {
            let n = v.to_number();
            if n.is_nan() { 0.0 } else { n.trunc() }
        }
    }
}

/// Resolve a relative index (negative counts from the end) into 0..=len.
fn rel_index(n: f64, len: usize) -> usize {
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        n.min(len as f64) as usize
    }
}

fn this_str(this: &Value) -> Rc<JsStr> {
    this.to_js_string()
}

pub fn to_int32(n: f64) -> i32 {
    if n.is_finite() {
        // Modular conversion, like ECMAScript ToInt32.
        let n = n.trunc() % 4294967296.0;
        (n as i64) as u32 as i32
    } else {
        0
    }
}

pub fn math1(n: Native, x: f64) -> f64 {
    match n {
        Native::MathFloor => x.floor(),
        Native::MathCeil => x.ceil(),
        Native::MathRound => {
            let r = (x + 0.5).floor();
            if r == 0.0 && x < 0.0 { -0.0 } else { r }
        }
        Native::MathTrunc => x.trunc(),
        Native::MathSqrt => x.sqrt(),
        Native::MathAbs => x.abs(),
        Native::MathSign => {
            if x.is_nan() || x == 0.0 {
                x
            } else {
                x.signum()
            }
        }
        Native::MathSin => x.sin(),
        Native::MathCos => x.cos(),
        Native::MathTan => x.tan(),
        Native::MathAsin => x.asin(),
        Native::MathAcos => x.acos(),
        Native::MathAtan => x.atan(),
        Native::MathExp => x.exp(),
        Native::MathLog => x.ln(),
        Native::MathLog2 => x.log2(),
        Native::MathLog10 => x.log10(),
        Native::MathCbrt => x.cbrt(),
        Native::MathFround => x as f32 as f64,
        Native::MathClz32 => (to_int32(x) as u32).leading_zeros() as f64,
        _ => unreachable!("not a 1-arg math native: {n:?}"),
    }
}

pub fn math2(n: Native, a: f64, b: f64) -> f64 {
    match n {
        Native::MathAtan2 => a.atan2(b),
        Native::MathPow => js_pow(a, b),
        Native::MathMin => {
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else {
                a.min(b)
            }
        }
        Native::MathMax => {
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else {
                a.max(b)
            }
        }
        Native::MathHypot => a.hypot(b),
        Native::MathImul => to_int32(a).wrapping_mul(to_int32(b)) as f64,
        _ => unreachable!("not a 2-arg math native: {n:?}"),
    }
}

pub fn js_pow(a: f64, b: f64) -> f64 {
    if b.is_nan() || ((a == 1.0 || a == -1.0) && b.is_infinite()) {
        f64::NAN
    } else {
        a.powf(b)
    }
}

pub fn js_mod(a: f64, b: f64) -> f64 {
    a % b
}

fn random() -> f64 {
    use std::cell::Cell;
    thread_local!(static STATE: Cell<u64> = Cell::new({
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        t | 1
    }));
    STATE.with(|s| {
        // xorshift64*
        let mut x = s.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        s.set(x);
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn parse_int(s: &str, radix: f64) -> f64 {
    let t = s.trim_start();
    let (neg, t) = if let Some(r) = t.strip_prefix('-') {
        (true, r)
    } else {
        (false, t.strip_prefix('+').unwrap_or(t))
    };
    let mut radix = if radix.is_nan() { 0 } else { radix as u32 };
    let mut t = t;
    if (radix == 0 || radix == 16) && (t.starts_with("0x") || t.starts_with("0X")) {
        t = &t[2..];
        radix = 16;
    }
    if radix == 0 {
        radix = 10;
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let digits: String = t.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut v = 0f64;
    for c in digits.chars() {
        v = v * radix as f64 + c.to_digit(radix).unwrap() as f64;
    }
    if neg { -v } else { v }
}

fn parse_float(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == ds || (i == ds + 1 && b[ds] == b'.') {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let save = i;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if i < b.len() && b[i].is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            i = save;
        }
    }
    t[..i].parse().unwrap_or(f64::NAN)
}

pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() {
        return num_to_string(x);
    }
    if x.abs() >= 1e21 {
        return num_to_string(x);
    }
    let s = format!("{:.*}", digits, x);
    if s.starts_with('-') && s[1..].chars().all(|c| c == '0' || c == '.') {
        return s[1..].to_string();
    }
    s
}

fn radix_string(mut x: f64, radix: u32) -> String {
    if radix == 10 || !x.is_finite() {
        return num_to_string(x);
    }
    let neg = x < 0.0;
    x = x.abs();
    let mut int = x.trunc();
    let mut frac = x - int;
    let mut digits = Vec::new();
    if int == 0.0 {
        digits.push('0');
    }
    while int > 0.0 {
        let d = (int % radix as f64) as u32;
        digits.push(std::char::from_digit(d, radix).unwrap());
        int = (int / radix as f64).trunc();
    }
    digits.reverse();
    let mut s: String = digits.into_iter().collect();
    if frac > 0.0 {
        s.push('.');
        let mut n = 0;
        while frac > 0.0 && n < 52 {
            frac *= radix as f64;
            let d = frac.trunc() as u32;
            s.push(std::char::from_digit(d, radix).unwrap());
            frac -= d as f64;
            n += 1;
        }
    }
    if neg { format!("-{s}") } else { s }
}

fn str_index_of(hay: &JsStr, needle: &str, from: usize) -> f64 {
    if hay.ascii {
        if from > hay.s.len() {
            return if needle.is_empty() { hay.s.len() as f64 } else { -1.0 };
        }
        return hay.s[from..].find(needle).map_or(-1.0, |i| (i + from) as f64);
    }
    let h = hay.utf16();
    let n: Vec<u16> = needle.encode_utf16().collect();
    if n.is_empty() {
        return from.min(h.len()) as f64;
    }
    (from..h.len().saturating_sub(n.len() - 1))
        .find(|&i| h[i..i + n.len()] == n[..])
        .map_or(-1.0, |i| i as f64)
}

fn arr_of(this: &Value) -> Result<Rc<Array>, String> {
    match this {
        Value::Arr(a) => Ok(a.clone()),
        v => Err(format!("{} is not an array", v.type_of())),
    }
}

/// Merge sort with a fallible comparator (stable, like V8's TimSort).
fn merge_sort(v: &mut Vec<Value>, cmp: &mut dyn FnMut(&Value, &Value) -> Result<bool, Value>) -> Result<(), Value> {
    let n = v.len();
    if n < 2 {
        return Ok(());
    }
    // Insertion sort for short runs.
    if n <= 10 {
        for i in 1..n {
            let mut j = i;
            while j > 0 && cmp(&v[j], &v[j - 1])? {
                v.swap(j, j - 1);
                j -= 1;
            }
        }
        return Ok(());
    }
    let mut right = v.split_off(n / 2);
    merge_sort(v, cmp)?;
    merge_sort(&mut right, cmp)?;
    let left = std::mem::take(v);
    let mut out = Vec::with_capacity(n);
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        // Take from the right only if strictly less (stability).
        if cmp(&right[j], &left[i])? {
            out.push(right[j].clone());
            j += 1;
        } else {
            out.push(left[i].clone());
            i += 1;
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    *v = out;
    Ok(())
}

/// Whether a callback wants the index/array args (avoid boxing them for 1-param callbacks).
fn cb_args(x: Value, i: usize, arr: &Rc<Array>) -> [Value; 3] {
    [x, Value::Num(i as f64), Value::Arr(arr.clone())]
}

/// Whether JSON.stringify would reach a BigInt (which it cannot serialize).
fn contains_bigint(v: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match v {
        Value::BigInt(_) => true,
        Value::Arr(a) => a.to_vec().iter().any(|x| contains_bigint(x, depth + 1)),
        Value::Obj(o) => o.keys().iter().any(|k| o.get(k).is_some_and(|x| contains_bigint(&x, depth + 1))),
        _ => false,
    }
}

pub fn json_stringify(v: &Value, indent: &str, cur: &str, out: &mut String) -> bool {
    match v {
        Value::Undef | Value::Func(_) | Value::Native(_) | Value::Class(_) | Value::Bound(_) | Value::Symbol(_) => return false,
        Value::Gen(_) | Value::Promise(_) | Value::Iter(_) | Value::Regex(_) => out.push_str("{}"),
        Value::Accessor(_) => return false,
        // JSON.stringify checks for BigInts (a TypeError) before getting here.
        Value::BigInt(n) => out.push_str(&n.to_string()),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => {
            if n.is_finite() {
                out.push_str(&num_to_string(*n))
            } else {
                out.push_str("null")
            }
        }
        Value::Str(s) => json_quote(&s.s, out),
        Value::Arr(a) => {
            let vals = a.to_vec();
            if vals.is_empty() {
                out.push_str("[]");
                return true;
            }
            let inner = format!("{cur}{indent}");
            out.push('[');
            for (i, x) in vals.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !indent.is_empty() {
                    out.push('\n');
                    out.push_str(&inner);
                }
                if !json_stringify(x, indent, &inner, out) {
                    out.push_str("null");
                }
            }
            if !indent.is_empty() {
                out.push('\n');
                out.push_str(cur);
            }
            out.push(']');
        }
        Value::Obj(o) if o.class.as_ref().is_some_and(|c| &*c.name == "Date") => match o.get("t") {
            Some(Value::Num(t)) if t.is_finite() => json_quote(&iso_time(t), out),
            _ => out.push_str("null"),
        },
        Value::Obj(o) => {
            let (keys, vals) = {
                let i = o.inner.borrow();
                (i.shape.keys.clone(), i.vals.clone())
            };
            let inner = format!("{cur}{indent}");
            out.push('{');
            let mut first = true;
            for (k, x) in keys.iter().zip(vals.iter()) {
                if task::is_symbol_key(k) {
                    continue;
                }
                let mark = out.len();
                if !first {
                    out.push(',');
                }
                if !indent.is_empty() {
                    out.push('\n');
                    out.push_str(&inner);
                }
                json_quote(k, out);
                out.push(':');
                if !indent.is_empty() {
                    out.push(' ');
                }
                if json_stringify(x, indent, &inner, out) {
                    first = false;
                } else {
                    out.truncate(mark);
                }
            }
            if !indent.is_empty() && !first {
                out.push('\n');
                out.push_str(cur);
            }
            out.push('}');
        }
        Value::Map(_) | Value::Set(_) => out.push_str("{}"),
        Value::Cell(c) => return json_stringify(&c.borrow(), indent, cur, out),
    }
    true
}

fn json_quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn is_callable_value(v: &Value) -> bool {
    matches!(v, Value::Func(_) | Value::Native(_) | Value::Bound(_))
}

/// Whether JSON.stringify needs the slow path (replacer or `toJSON`).
fn json_needs_prepare(args: &[Value]) -> bool {
    fn has_to_json(v: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match v {
            Value::Obj(o) => {
                o.get("toJSON").is_some()
                    || o.class.as_ref().is_some_and(|c| c.methods.contains_key("toJSON") && &*c.name != "Date")
                    || o.keys().iter().any(|k| o.get(k).is_some_and(|x| has_to_json(&x, depth + 1)))
            }
            Value::Arr(a) => a.to_vec().iter().any(|x| has_to_json(x, depth + 1)),
            _ => false,
        }
    }
    matches!(args.get(1), Some(Value::Func(_) | Value::Native(_) | Value::Bound(_) | Value::Arr(_))) || has_to_json(&arg(args, 0), 0)
}

/// SerializeJSONProperty's value step: `toJSON`, then the replacer, then
/// the same for children. None = omit.
fn json_prepare(vm: &mut Vm, holder: &Value, key: Value, v: Value, replacer: &Value, depth: usize) -> Result<Option<Value>, Value> {
    if depth > 1000 {
        return Err(vm.range_error("Maximum call stack size exceeded".into()));
    }
    let mut v = v;
    if let Value::Obj(_) = &v {
        let f = vm.get_property(&v, "toJSON")?;
        if is_callable_value(&f) {
            v = vm.call(&f, v.clone(), vec![key.clone()])?;
        }
    }
    if is_callable_value(replacer) {
        v = vm.call(replacer, holder.clone(), vec![key, v])?;
    }
    Ok(Some(match &v {
        Value::Arr(a) => {
            let mut out = Vec::new();
            for (i, x) in a.to_vec().into_iter().enumerate() {
                out.push(json_prepare(vm, &v, Value::str(i.to_string()), x, replacer, depth + 1)?.unwrap_or(Value::Null));
            }
            Value::Arr(make_array(out))
        }
        Value::Obj(o) => {
            let allowed: Option<Vec<String>> = match replacer {
                Value::Arr(list) => Some(list.to_vec().iter().map(|k| k.to_js_string().s.to_string()).collect()),
                _ => None,
            };
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            let names: Vec<Rc<str>> = match &allowed {
                Some(list) => list.iter().filter(|k| o.get(k).is_some()).map(|k| Rc::from(k.as_str())).collect(),
                None => object_keys(o),
            };
            for k in names {
                let x = o.get(&k).unwrap_or(Value::Undef);
                if let Some(x) = json_prepare(vm, &v, Value::Str(JsStr::new(&*k)), x, replacer, depth + 1)? {
                    if matches!(x, Value::Undef | Value::Func(_) | Value::Native(_)) {
                        continue;
                    }
                    keys.push(k);
                    vals.push(x);
                }
            }
            Value::Obj(Object::new(Shape::new(keys), vals, None))
        }
        Value::Undef | Value::Func(_) | Value::Native(_) | Value::Bound(_) => return Ok(None),
        _ => v.clone(),
    }))
}

/// JSON.parse's reviver, applied bottom-up.
fn json_revive(vm: &mut Vm, holder: &Value, key: Value, reviver: &Value) -> Result<Value, Value> {
    let v = get_dyn(holder, &key);
    match &v {
        Value::Arr(a) => {
            for i in 0..a.len() {
                let k = Value::str(i.to_string());
                let x = json_revive(vm, &v, k.clone(), reviver)?;
                set_dyn(&v, &Value::Num(i as f64), x);
            }
        }
        Value::Obj(o) => {
            for k in o.keys() {
                let kv = Value::Str(JsStr::new(&*k));
                let x = json_revive(vm, &v, kv, reviver)?;
                if matches!(x, Value::Undef) {
                    o.delete(&k);
                } else {
                    o.set(&k, x);
                }
            }
        }
        _ => {}
    }
    vm.call(reviver, holder.clone(), vec![key, v])
}

/// Parse JSON text (used by the module loader for package.json).
pub fn parse_json(s: &str) -> Option<Value> {
    let mut p = JsonParser { b: s.as_bytes(), s, i: 0 };
    let v = p.value().ok()?;
    p.ws();
    (p.i == p.b.len()).then_some(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }
    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        let Some(&c) = self.b.get(self.i) else {
            return Err("Unexpected end of JSON input".into());
        };
        match c {
            b'{' => {
                self.i += 1;
                let mut keys: Vec<Rc<str>> = Vec::new();
                let mut vals = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.ws();
                        if self.b.get(self.i) != Some(&b':') {
                            return Err(format!("Expected ':' in JSON at position {}", self.i));
                        }
                        self.i += 1;
                        let v = self.value()?;
                        if let Some(p) = keys.iter().position(|x| **x == *k) {
                            vals[p] = v;
                        } else {
                            keys.push(k.into());
                            vals.push(v);
                        }
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(format!("Unexpected token in JSON at position {}", self.i)),
                        }
                    }
                }
                Ok(Value::Obj(Object::new(Shape::new(keys), vals, None)))
            }
            b'[' => {
                self.i += 1;
                let mut vals = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        vals.push(self.value()?);
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(format!("Unexpected token in JSON at position {}", self.i)),
                        }
                    }
                }
                Ok(Value::Arr(Array::new_val(vals)))
            }
            b'"' => Ok(Value::str(self.string()?)),
            b't' if self.s[self.i..].starts_with("true") => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            b'f' if self.s[self.i..].starts_with("false") => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            b'n' if self.s[self.i..].starts_with("null") => {
                self.i += 4;
                Ok(Value::Null)
            }
            b'-' | b'0'..=b'9' => {
                let st = self.i;
                self.i += 1;
                while self.i < self.b.len() && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    self.i += 1;
                }
                self.s[st..self.i]
                    .parse()
                    .map(Value::Num)
                    .map_err(|_| format!("Unexpected number in JSON at position {st}"))
            }
            _ => Err(format!("Unexpected token '{}', \"{}\" is not valid JSON", c as char, self.s)),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(format!("Expected string in JSON at position {}", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err("Unterminated string in JSON".into());
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.b.get(self.i).copied().unwrap_or(0);
                    self.i += 1;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let h = self.s.get(self.i..self.i + 4).unwrap_or("");
                            let v = u32::from_str_radix(h, 16).map_err(|_| "Bad unicode escape in JSON")?;
                            self.i += 4;
                            out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        }
                        c => out.push(c as char),
                    }
                }
                _ => {
                    let ch = self.s[self.i..].chars().next().unwrap();
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }
}

/// `Object.keys` order: integer-like keys ascending first, then insertion order.
fn object_keys(o: &Object) -> Vec<Rc<str>> {
    let keys = o.keys();
    let mut ints: Vec<(u32, Rc<str>)> = Vec::new();
    let mut rest = Vec::new();
    for k in keys {
        if task::is_symbol_key(&k) {
            continue;
        }
        match k.parse::<u32>() {
            Ok(n) if n.to_string() == *k => ints.push((n, k)),
            _ => rest.push(k),
        }
    }
    ints.sort_by_key(|(n, _)| *n);
    ints.into_iter().map(|(_, k)| k).chain(rest).collect()
}

pub fn keys_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Obj(o) => object_keys(o).into_iter().map(|k| Value::Str(JsStr::new(&*k))).collect(),
        Value::Arr(a) => (0..a.len()).map(|i| Value::str(i.to_string())).collect(),
        Value::Str(s) => (0..s.len16()).map(|i| Value::str(i.to_string())).collect(),
        _ => vec![],
    }
}

/// The elements a `for-of` loop or spread visits.
pub fn iterate(vm: &mut Vm, v: &Value) -> Result<Vec<Value>, Value> {
    Ok(match v {
        Value::Arr(a) => a.to_vec(),
        Value::Str(s) => s.s.chars().map(|c| Value::str(c.to_string())).collect(),
        Value::Map(m) => m
            .borrow()
            .iter()
            .map(|(k, x)| Value::Arr(Array::new_val(vec![k.0.clone(), x.clone()])))
            .collect(),
        Value::Set(s) => s.borrow().iter().map(|k| k.0.clone()).collect(),
        Value::Iter(i) => i.rest(),
        Value::Gen(_) | Value::Obj(_) => {
            let it = vm.get_iterator(v)?;
            let mut out = Vec::new();
            while let Some(x) = vm.iterator_step(&it)? {
                out.push(x);
            }
            out
        }
        v => return Err(vm.type_error(format!("{} is not iterable", v.type_of()))),
    })
}

pub fn call_native(vm: &mut Vm, n: Native, this: Value, args: Vec<Value>) -> NResult {
    use Native::*;
    fn te<T>(vm: &mut Vm, m: String) -> Result<T, Value> {
        Err(vm.type_error(m))
    }
    Ok(match n {
        n if n.is_task() => return crate::vm::call_task_native(vm, n, this, args),
        n if (n as u16) >= (ProcessObject as u16) => return crate::node::natives::call_more(vm, n, args),
        ConsoleLog | ConsoleError | ConsoleWarn => {
            let line = crate::node::natives::format_args(vm, &args);
            if n == ConsoleLog {
                vm.print(&line);
            } else {
                vm.eprint(&line);
            }
            Value::Undef
        }
        MathFloor | MathCeil | MathRound | MathTrunc | MathSqrt | MathAbs | MathSign | MathSin | MathCos
        | MathTan | MathAsin | MathAcos | MathAtan | MathExp | MathLog | MathLog2 | MathLog10 | MathCbrt
        | MathFround | MathClz32 => Value::Num(math1(n, num(&args, 0))),
        MathAtan2 | MathPow | MathImul => Value::Num(math2(n, num(&args, 0), num(&args, 1))),
        MathMin | MathMax | MathHypot => {
            let mut acc = match n {
                MathMin => f64::INFINITY,
                MathMax => f64::NEG_INFINITY,
                _ => 0.0,
            };
            if n == MathHypot {
                let s: f64 = args.iter().map(|a| a.to_number().powi(2)).sum();
                return Ok(Value::Num(s.sqrt()));
            }
            for a in &args {
                acc = math2(n, acc, a.to_number());
            }
            Value::Num(acc)
        }
        MathRandom => Value::Num(random()),
        ParseInt => Value::Num(parse_int(&arg(&args, 0).to_js_string().s, num(&args, 1))),
        ParseFloat => Value::Num(parse_float(&arg(&args, 0).to_js_string().s)),
        IsNaN => Value::Bool(num(&args, 0).is_nan()),
        IsFinite => Value::Bool(num(&args, 0).is_finite()),
        StringFn => match args.first() {
            None => Value::str(""),
            Some(Value::Symbol(sym)) => Value::str(format!("Symbol({})", sym.description.as_deref().unwrap_or(""))),
            Some(v) => Value::Str(vm.to_string_value(v)?),
        },
        NumberFn => Value::Num(args.first().map_or(0.0, |v| v.to_number())),
        BooleanFn => Value::Bool(arg(&args, 0).truthy()),
        BigIntFn => {
            let r = match arg(&args, 0) {
                Value::BigInt(n) => Ok(*n),
                Value::Num(x) if x.fract() != 0.0 || !x.is_finite() => {
                    return Err(vm.range_error(format!("The number {} cannot be converted to a BigInt because it is not an integer", num_to_string(x))));
                }
                Value::Num(x) => bigint::from_number(x),
                Value::Bool(b) => Ok(b as i128),
                Value::Str(s) => match bigint::parse(&s.s) {
                    Some(n) => Ok(n),
                    None => return Err(vm.syntax_error(format!("Cannot convert {} to a BigInt", s.s))),
                },
                v => return te(vm, format!("Cannot convert {} to a BigInt", v.to_display())),
            };
            match r {
                Ok(n) => Value::bigint(n),
                Err(e) => return Err(vm.bigint_error(e)),
            }
        }
        NumIsInteger => Value::Bool(matches!(arg(&args, 0), Value::Num(x) if x.is_finite() && x.fract() == 0.0)),
        NumIsSafeInteger => {
            Value::Bool(matches!(arg(&args, 0), Value::Num(x) if x.is_finite() && x.fract() == 0.0 && x.abs() <= 9007199254740991.0))
        }
        NumIsFinite => Value::Bool(matches!(arg(&args, 0), Value::Num(x) if x.is_finite())),
        NumIsNaN => Value::Bool(matches!(arg(&args, 0), Value::Num(x) if x.is_nan())),
        NumToFixed => {
            let d = int_arg(&args, 0, 0.0);
            if !(0.0..=100.0).contains(&d) {
                return Err(vm.range_error("toFixed() digits argument must be between 0 and 100".into()));
            }
            Value::str(to_fixed(this.to_number(), d as usize))
        }
        NumToPrecision => {
            let x = this.to_number();
            match args.first() {
                None | Some(Value::Undef) => Value::str(num_to_string(x)),
                Some(p) => {
                    let p = p.to_number() as i32;
                    if x == 0.0 || !x.is_finite() {
                        return Ok(Value::str(if x == 0.0 { to_fixed(0.0, (p - 1).max(0) as usize) } else { num_to_string(x) }));
                    }
                    let e = x.abs().log10().floor() as i32;
                    if e < -6 || e >= p {
                        let s = format!("{:.*e}", (p - 1).max(0) as usize, x);
                        let (m, ex) = s.split_once('e').unwrap();
                        let ex: i32 = ex.parse().unwrap();
                        Value::str(format!("{m}e{}{}", if ex >= 0 { "+" } else { "-" }, ex.abs()))
                    } else {
                        Value::str(to_fixed(x, (p - 1 - e).max(0) as usize))
                    }
                }
            }
        }
        NumToString => {
            let r = int_arg(&args, 0, 10.0) as u32;
            if !(2..=36).contains(&r) {
                return Err(vm.range_error("toString() radix must be between 2 and 36".into()));
            }
            match &this {
                Value::Num(x) => Value::str(radix_string(*x, r)),
                Value::BigInt(n) => Value::str(bigint::to_string_radix(**n, r)),
                v => Value::Str(v.to_js_string()),
            }
        }

        // ───── strings ─────
        StrCharAt => {
            let s = this_str(&this);
            let i = int_arg(&args, 0, 0.0);
            if i < 0.0 {
                Value::str("")
            } else {
                Value::str(s.slice16(i as usize, i as usize + 1))
            }
        }
        StrCharCodeAt => {
            let s = this_str(&this);
            let i = int_arg(&args, 0, 0.0);
            if i < 0.0 {
                Value::Num(f64::NAN)
            } else {
                Value::Num(s.code_unit(i as usize).map_or(f64::NAN, |c| c as f64))
            }
        }
        StrCodePointAt => {
            let s = this_str(&this);
            let i = int_arg(&args, 0, 0.0);
            let u = s.utf16();
            if i < 0.0 || i as usize >= u.len() {
                Value::Undef
            } else {
                let i = i as usize;
                let c = char::decode_utf16(u[i..].iter().copied()).next().unwrap();
                Value::Num(match c {
                    Ok(c) => c as u32 as f64,
                    Err(e) => e.unpaired_surrogate() as f64,
                })
            }
        }
        StrIndexOf => {
            let s = this_str(&this);
            let needle = arg(&args, 0).to_js_string();
            let from = int_arg(&args, 1, 0.0).max(0.0) as usize;
            Value::Num(str_index_of(&s, &needle.s, from))
        }
        StrLastIndexOf => {
            let s = this_str(&this);
            let needle = arg(&args, 0).to_js_string();
            let h = s.utf16();
            let n: Vec<u16> = needle.s.encode_utf16().collect();
            let from = match args.get(1) {
                Some(v) if !v.to_number().is_nan() => (v.to_number().max(0.0) as usize).min(h.len()),
                _ => h.len(),
            };
            let mut r = -1.0;
            if n.len() <= h.len() {
                let mut i = from.min(h.len() - n.len()) as isize;
                while i >= 0 {
                    if h[i as usize..i as usize + n.len()] == n[..] {
                        r = i as f64;
                        break;
                    }
                    i -= 1;
                }
            }
            Value::Num(r)
        }
        StrIncludes => {
            let s = this_str(&this);
            let needle = arg(&args, 0).to_js_string();
            let from = int_arg(&args, 1, 0.0).max(0.0) as usize;
            Value::Bool(str_index_of(&s, &needle.s, from) >= 0.0)
        }
        StrStartsWith => {
            let s = this_str(&this);
            let needle = arg(&args, 0).to_js_string();
            let pos = int_arg(&args, 1, 0.0).max(0.0) as usize;
            let rest = s.slice16(pos, usize::MAX);
            Value::Bool(rest.starts_with(&*needle.s))
        }
        StrEndsWith => {
            let s = this_str(&this);
            let needle = arg(&args, 0).to_js_string();
            let end = int_arg(&args, 1, s.len16() as f64).max(0.0) as usize;
            Value::Bool(s.slice16(0, end).ends_with(&*needle.s))
        }
        StrSlice | StrSubstring | StrSubstr => {
            let s = this_str(&this);
            let len = s.len16();
            let (a, b) = match n {
                StrSlice => (
                    rel_index(int_arg(&args, 0, 0.0), len),
                    rel_index(int_arg(&args, 1, len as f64), len),
                ),
                StrSubstring => {
                    let a = int_arg(&args, 0, 0.0).clamp(0.0, len as f64) as usize;
                    let b = int_arg(&args, 1, len as f64).clamp(0.0, len as f64) as usize;
                    (a.min(b), a.max(b))
                }
                _ => {
                    let a = rel_index(int_arg(&args, 0, 0.0), len);
                    let l = int_arg(&args, 1, (len - a) as f64).max(0.0) as usize;
                    (a, (a + l).min(len))
                }
            };
            if a >= b {
                Value::str("")
            } else {
                Value::str(s.slice16(a, b))
            }
        }
        StrToUpper => Value::str(this_str(&this).s.to_uppercase()),
        StrToLower => Value::str(this_str(&this).s.to_lowercase()),
        StrTrim => Value::str(this_str(&this).s.trim()),
        StrTrimStart => Value::str(this_str(&this).s.trim_start()),
        StrTrimEnd => Value::str(this_str(&this).s.trim_end()),
        StrSplit => {
            let s = this_str(&this);
            let limit = match args.get(1) {
                Some(Value::Num(n)) => *n as usize,
                _ => usize::MAX,
            };
            let parts: Vec<Value> = match args.first() {
                None | Some(Value::Undef) => vec![Value::Str(s.clone())],
                Some(Value::Regex(re)) => regex_split(re, &s),
                Some(sep) => {
                    let sep = sep.to_js_string();
                    if sep.s.is_empty() {
                        s.s.chars().map(|c| Value::str(c.to_string())).collect()
                    } else {
                        s.s.split(&*sep.s).map(Value::str).collect()
                    }
                }
            };
            Value::Arr(Array::new_val(parts.into_iter().take(limit).collect()))
        }
        StrRepeat => {
            let c = num(&args, 0);
            if c < 0.0 || c.is_infinite() {
                return Err(vm.range_error(format!("Invalid count value: {}", num_to_string(c))));
            }
            Value::str(this_str(&this).s.repeat(c as usize))
        }
        StrPadStart | StrPadEnd => {
            let s = this_str(&this);
            let target = int_arg(&args, 0, 0.0).max(0.0) as usize;
            let fill = match args.get(1) {
                None | Some(Value::Undef) => " ".to_string(),
                Some(v) => v.to_js_string().s.to_string(),
            };
            let len = s.len16();
            if target <= len || fill.is_empty() {
                Value::Str(s)
            } else {
                let need = target - len;
                let pad: String = fill.chars().cycle().take(need).collect();
                if n == StrPadStart {
                    Value::str(format!("{pad}{}", s.s))
                } else {
                    Value::str(format!("{}{pad}", s.s))
                }
            }
        }
        StrReplace | StrReplaceAll if matches!(args.first(), Some(Value::Regex(_))) => {
            let Some(Value::Regex(re)) = args.first() else { unreachable!() };
            if n == StrReplaceAll && !re.global {
                return te(vm, "replaceAll must be called with a global RegExp".into());
            }
            let s = this_str(&this);
            regex_replace(vm, re, &s, &arg(&args, 1))?
        }
        StrReplace | StrReplaceAll => {
            let s = this_str(&this);
            let pat = arg(&args, 0).to_js_string();
            let rep = arg(&args, 1);
            let all = n == StrReplaceAll;
            let mut out = String::new();
            let mut rest: &str = &s.s;
            let mut offset = 0usize;
            loop {
                let Some(i) = rest.find(&*pat.s) else {
                    out.push_str(rest);
                    break;
                };
                out.push_str(&rest[..i]);
                match &rep {
                    Value::Func(_) | Value::Native(_) => {
                        let r = vm.call(&rep, Value::Undef, vec![Value::Str(pat.clone()), Value::Num((offset + i) as f64), Value::Str(s.clone())])?;
                        out.push_str(&r.to_js_string().s);
                    }
                    r => out.push_str(&r.to_js_string().s.replace("$&", &pat.s)),
                }
                let adv = i + pat.s.len();
                if pat.s.is_empty() {
                    // Empty pattern: insert between every char (replaceAll) or once at start.
                    if !all || rest.is_empty() {
                        out.push_str(rest);
                        break;
                    }
                    let c = rest.chars().next().unwrap();
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                    offset += c.len_utf8();
                    continue;
                }
                rest = &rest[adv..];
                offset += adv;
                if !all {
                    out.push_str(rest);
                    break;
                }
            }
            Value::str(out)
        }
        StrConcat => {
            let mut s = this_str(&this).s.to_string();
            for a in &args {
                s.push_str(&a.to_js_string().s);
            }
            Value::str(s)
        }
        StrAt => {
            let s = this_str(&this);
            let len = s.len16() as f64;
            let mut i = int_arg(&args, 0, 0.0);
            if i < 0.0 {
                i += len;
            }
            if i < 0.0 || i >= len {
                Value::Undef
            } else {
                Value::str(s.slice16(i as usize, i as usize + 1))
            }
        }
        StrLocaleCompare => {
            let a = this_str(&this);
            let b = arg(&args, 0).to_js_string();
            Value::Num(match a.s.cmp(&b.s) {
                std::cmp::Ordering::Less => -1.0,
                std::cmp::Ordering::Equal => 0.0,
                std::cmp::Ordering::Greater => 1.0,
            })
        }
        StrFromCharCode => {
            let units: Vec<u16> = args.iter().map(|a| to_int32(a.to_number()) as u16).collect();
            Value::str(String::from_utf16_lossy(&units))
        }
        StrToString | StrNormalize => Value::Str(this_str(&this)),

        // ───── arrays ─────
        ArrPush => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            for x in args {
                a.push(x);
            }
            Value::Num(a.len() as f64)
        }
        ArrPop => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let mut st = a.store.borrow_mut();
            match &mut *st {
                ArrStore::Num(v) => v.pop().map_or(Value::Undef, Value::Num),
                ArrStore::Val(v) => v.pop().unwrap_or(Value::Undef),
            }
        }
        ArrShift => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let mut st = a.store.borrow_mut();
            match &mut *st {
                ArrStore::Num(v) if !v.is_empty() => Value::Num(v.remove(0)),
                ArrStore::Val(v) if !v.is_empty() => v.remove(0),
                _ => Value::Undef,
            }
        }
        ArrUnshift => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let mut vals = args;
            vals.extend(a.to_vec());
            replace_contents(&a, vals);
            Value::Num(a.len() as f64)
        }
        ArrSlice => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let len = a.len();
            let s = rel_index(int_arg(&args, 0, 0.0), len);
            let e = rel_index(int_arg(&args, 1, len as f64), len);
            let st = a.store.borrow();
            Value::Arr(match &*st {
                ArrStore::Num(v) => Array::new_num(if s < e { v[s..e].to_vec() } else { vec![] }),
                ArrStore::Val(v) => Array::new_val(if s < e { v[s..e].to_vec() } else { vec![] }),
            })
        }
        ArrSplice => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let mut vals = a.to_vec();
            let len = vals.len();
            let start = rel_index(int_arg(&args, 0, 0.0), len);
            let del = if args.len() < 2 {
                len - start
            } else {
                (int_arg(&args, 1, 0.0).max(0.0) as usize).min(len - start)
            };
            let inserted: Vec<Value> = args.into_iter().skip(2).collect();
            let removed: Vec<Value> = vals.splice(start..start + del, inserted).collect();
            replace_contents(&a, vals);
            Value::Arr(Array::new_val(removed))
        }
        ArrConcat => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let mut vals = a.to_vec();
            for x in args {
                match x {
                    Value::Arr(b) => vals.extend(b.to_vec()),
                    x => vals.push(x),
                }
            }
            Value::Arr(make_array(vals))
        }
        ArrJoin => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let sep = match args.first() {
                None | Some(Value::Undef) => ",".to_string(),
                Some(v) => v.to_js_string().s.to_string(),
            };
            let st = a.store.borrow();
            let s = match &*st {
                ArrStore::Num(v) => v.iter().map(|n| num_to_string(*n)).collect::<Vec<_>>().join(&sep),
                ArrStore::Val(v) => v
                    .iter()
                    .map(|x| if x.is_nullish() { String::new() } else { x.to_display() })
                    .collect::<Vec<_>>()
                    .join(&sep),
            };
            Value::str(s)
        }
        ArrToString => Value::str(this.to_display()),
        ArrToSorted | ArrToReversed | ArrWith => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let copy = Value::Arr(make_array(a.to_vec()));
            match n {
                ArrToSorted => call_native(vm, ArrSort, copy, args)?,
                ArrToReversed => call_native(vm, ArrReverse, copy, vec![])?,
                _ => {
                    let len = a.len();
                    let i = int_arg(&args, 0, 0.0);
                    let i = if i < 0.0 { len as f64 + i } else { i };
                    if i < 0.0 || i >= len as f64 {
                        return Err(vm.range_error(format!("Invalid index : {}", num_to_string(int_arg(&args, 0, 0.0)))));
                    }
                    set_dyn(&copy, &Value::Num(i), arg(&args, 1));
                    copy
                }
            }
        }
        StructuredClone => deep_clone(&arg(&args, 0)),
        GlobalThis => {
            let names = ["setTimeout", "setInterval", "clearTimeout", "clearInterval", "queueMicrotask", "structuredClone"];
            let fns = [SetTimeout, SetInterval, ClearTimeout, ClearTimeout, QueueMicrotask, StructuredClone];
            Value::Obj(Object::new(
                Shape::new(names.iter().map(|n| Rc::from(*n)).collect()),
                fns.iter().map(|f| Value::Native(*f)).collect(),
                None,
            ))
        }
        ArrReverse => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            match &mut *a.store.borrow_mut() {
                ArrStore::Num(v) => v.reverse(),
                ArrStore::Val(v) => v.reverse(),
            }
            Value::Arr(a)
        }
        ArrIndexOf | ArrLastIndexOf | ArrIncludes => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let x = arg(&args, 0);
            let vals = a.to_vec();
            let found = if n == ArrLastIndexOf {
                vals.iter().rposition(|v| v.strict_eq(&x))
            } else if n == ArrIncludes {
                vals.iter().position(|v| HKey(v.clone()) == HKey(x.clone()))
            } else {
                let from = rel_index(int_arg(&args, 1, 0.0), vals.len());
                vals.iter().skip(from).position(|v| v.strict_eq(&x)).map(|i| i + from)
            };
            if n == ArrIncludes {
                Value::Bool(found.is_some())
            } else {
                Value::Num(found.map_or(-1.0, |i| i as f64))
            }
        }
        ArrMap | ArrFilter | ArrForEach | ArrFind | ArrFindIndex | ArrFindLast | ArrFindLastIndex | ArrSome
        | ArrEvery | ArrFlatMap => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let f = arg(&args, 0);
            let len = a.len();
            // Results start as number storage and degrade on the first non-number.
            let out = Array::new_num(Vec::new());
            let order: Vec<usize> = if matches!(n, ArrFindLast | ArrFindLastIndex) {
                (0..len).rev().collect()
            } else {
                (0..len).collect()
            };
            for i in order {
                if i >= a.len() {
                    break;
                }
                let x = a.get(i);
                let r = vm.call_slice(&f, Value::Undef, &cb_args(x.clone(), i, &a))?;
                match n {
                    ArrMap => out.push(r),
                    ArrFlatMap => match r {
                        Value::Arr(b) => {
                            for y in b.to_vec() {
                                out.push(y)
                            }
                        }
                        r => out.push(r),
                    },
                    ArrFilter => {
                        if r.truthy() {
                            out.push(x)
                        }
                    }
                    ArrForEach => {}
                    ArrFind | ArrFindLast => {
                        if r.truthy() {
                            return Ok(x);
                        }
                    }
                    ArrFindIndex | ArrFindLastIndex => {
                        if r.truthy() {
                            return Ok(Value::Num(i as f64));
                        }
                    }
                    ArrSome => {
                        if r.truthy() {
                            return Ok(Value::Bool(true));
                        }
                    }
                    ArrEvery => {
                        if !r.truthy() {
                            return Ok(Value::Bool(false));
                        }
                    }
                    _ => unreachable!(),
                }
            }
            match n {
                ArrMap | ArrFilter | ArrFlatMap => Value::Arr(out),
                ArrForEach | ArrFind | ArrFindLast => Value::Undef,
                ArrFindIndex | ArrFindLastIndex => Value::Num(-1.0),
                ArrSome => Value::Bool(false),
                _ => Value::Bool(true),
            }
        }
        ArrReduce | ArrReduceRight => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let f = arg(&args, 0);
            let len = a.len();
            let mut idx: Vec<usize> = (0..len).collect();
            if n == ArrReduceRight {
                idx.reverse();
            }
            let mut it = idx.into_iter();
            let mut acc = if args.len() >= 2 {
                args[1].clone()
            } else {
                match it.next() {
                    Some(i) => a.get(i),
                    None => return te(vm, "Reduce of empty array with no initial value".into()),
                }
            };
            for i in it {
                let x = a.get(i);
                acc = vm.call_slice(&f, Value::Undef, &[acc, x, Value::Num(i as f64), Value::Arr(a.clone())])?;
            }
            acc
        }
        ArrSort => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let f = arg(&args, 0);
            // Fast path: numeric store with no comparator still sorts by string.
            let mut vals = a.to_vec();
            let undef_count = vals.iter().filter(|v| matches!(v, Value::Undef)).count();
            vals.retain(|v| !matches!(v, Value::Undef));
            if matches!(f, Value::Undef) {
                let mut keyed: Vec<(String, Value)> = vals.into_iter().map(|v| (v.to_display(), v)).collect();
                keyed.sort_by(|x, y| x.0.encode_utf16().cmp(y.0.encode_utf16()));
                vals = keyed.into_iter().map(|(_, v)| v).collect();
            } else {
                merge_sort(&mut vals, &mut |x, y| {
                    let r = vm.call_slice(&f, Value::Undef, &[x.clone(), y.clone()])?;
                    Ok(r.to_number() < 0.0)
                })?;
            }
            vals.extend(std::iter::repeat_n(Value::Undef, undef_count));
            replace_contents(&a, vals);
            Value::Arr(a)
        }
        ArrFill => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let x = arg(&args, 0);
            let len = a.len();
            let s = rel_index(int_arg(&args, 1, 0.0), len);
            let e = rel_index(int_arg(&args, 2, len as f64), len);
            if let (Value::Num(v), true) = (&x, s == 0 && e == len) {
                *a.store.borrow_mut() = ArrStore::Num(vec![*v; len]);
            } else {
                for i in s..e {
                    a.set(i, x.clone());
                }
            }
            Value::Arr(a)
        }
        ArrAt => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let len = a.len() as f64;
            let mut i = int_arg(&args, 0, 0.0);
            if i < 0.0 {
                i += len;
            }
            if i < 0.0 || i >= len { Value::Undef } else { a.get(i as usize) }
        }
        ArrFlat => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let depth = int_arg(&args, 0, 1.0);
            fn flat(v: Vec<Value>, d: f64, out: &mut Vec<Value>) {
                for x in v {
                    match x {
                        Value::Arr(b) if d >= 1.0 => flat(b.to_vec(), d - 1.0, out),
                        x => out.push(x),
                    }
                }
            }
            let mut out = Vec::new();
            flat(a.to_vec(), depth, &mut out);
            Value::Arr(make_array(out))
        }
        ArrKeys => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            Value::Arr(Array::new_num((0..a.len()).map(|i| i as f64).collect()))
        }
        ArrEntries => {
            let a = arr_of(&this).or_else(|m| te(vm, m))?;
            let out = a
                .to_vec()
                .into_iter()
                .enumerate()
                .map(|(i, x)| Value::Arr(Array::new_val(vec![Value::Num(i as f64), x])))
                .collect();
            Value::Arr(Array::new_val(out))
        }
        ArrIsArray => Value::Bool(matches!(arg(&args, 0), Value::Arr(_))),
        ArrFrom => {
            let src = arg(&args, 0);
            let items = match &src {
                Value::Obj(o) => {
                    // Array.from({ length: n })
                    let n = o.get("length").map_or(0.0, |v| v.to_number());
                    vec![Value::Undef; n.max(0.0) as usize]
                }
                v => iterate(vm, v)?,
            };
            match args.get(1) {
                Some(f) if !matches!(f, Value::Undef) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (i, x) in items.into_iter().enumerate() {
                        out.push(vm.call(f, Value::Undef, vec![x, Value::Num(i as f64)])?);
                    }
                    Value::Arr(make_array(out))
                }
                _ => Value::Arr(make_array(items)),
            }
        }
        ArrOf => Value::Arr(make_array(args)),
        ArrNew => match args.as_slice() {
            [Value::Num(n)] => {
                if *n < 0.0 || n.fract() != 0.0 || *n > 4294967295.0 {
                    return Err(vm.range_error("Invalid array length".into()));
                }
                Value::Arr(Array::new_val(vec![Value::Undef; *n as usize]))
            }
            _ => Value::Arr(make_array(args)),
        },

        // ───── Object / JSON ─────
        ObjKeys => Value::Arr(Array::new_val(keys_of(&arg(&args, 0)))),
        ObjValues | ObjEntries => {
            let o = arg(&args, 0);
            let keys = keys_of(&o);
            let mut out = Vec::new();
            for k in keys {
                let v = get_dyn(&o, &k);
                out.push(if n == ObjValues { v } else { Value::Arr(Array::new_val(vec![k, v])) });
            }
            Value::Arr(make_array(out))
        }
        ObjAssign => {
            let target = arg(&args, 0);
            for src in args.iter().skip(1) {
                for k in keys_of(src) {
                    let v = get_dyn(src, &k);
                    set_dyn(&target, &k, v);
                }
            }
            target
        }
        ObjFreeze => arg(&args, 0),
        ObjFromEntries => {
            let o = Object::new(Shape::new(vec![]), vec![], None);
            for e in iterate(vm, &arg(&args, 0))? {
                if let Value::Arr(p) = e {
                    o.set(&p.get(0).to_js_string().s.clone().into(), p.get(1));
                }
            }
            Value::Obj(o)
        }
        JsonStringify if json_needs_prepare(&args) => {
            // A replacer or toJSON methods: build the value to serialize first.
            let replacer = arg(&args, 1);
            let root = Value::Obj(Object::new(Shape::new(vec!["".into()]), vec![arg(&args, 0)], None));
            let v = json_prepare(vm, &root, Value::str(""), arg(&args, 0), &replacer, 0)?;
            let mut rest = args.clone();
            rest[0] = v.unwrap_or(Value::Undef);
            if rest.len() > 1 {
                rest[1] = Value::Undef;
            }
            call_native(vm, JsonStringify, Value::Undef, rest)?
        }
        JsonStringify => {
            let indent = match args.get(2) {
                Some(Value::Num(n)) => " ".repeat((*n as usize).min(10)),
                Some(Value::Str(s)) => s.s.chars().take(10).collect(),
                _ => String::new(),
            };
            if contains_bigint(&arg(&args, 0), 0) {
                return te(vm, "Do not know how to serialize a BigInt".into());
            }
            let mut out = String::new();
            if json_stringify(&arg(&args, 0), &indent, "", &mut out) {
                Value::str(out)
            } else {
                Value::Undef
            }
        }
        JsonParse => {
            let s = arg(&args, 0).to_js_string();
            let mut p = JsonParser { b: s.s.as_bytes(), s: &s.s, i: 0 };
            let v = p.value();
            p.ws();
            match v {
                Ok(v) if p.i == p.b.len() && is_callable_value(&arg(&args, 1)) => {
                    let root = Value::Obj(Object::new(Shape::new(vec!["".into()]), vec![v.clone()], None));
                    json_revive(vm, &root, Value::str(""), &arg(&args, 1))?
                }
                Ok(v) if p.i == p.b.len() => v,
                Ok(_) => return Err(vm.syntax_error(format!("Unexpected non-whitespace character after JSON at position {}", p.i))),
                Err(m) => return Err(vm.syntax_error(m)),
            }
        }

        // ───── Map / Set ─────
        MapNew => {
            let m: IndexMap<HKey, Value, FxBuildHasher> = IndexMap::default();
            let m = Rc::new(RefCell::new(m));
            if let Some(init) = args.first().filter(|v| !v.is_nullish()) {
                for e in iterate(vm, init)? {
                    if let Value::Arr(p) = e {
                        m.borrow_mut().insert(HKey(p.get(0)), p.get(1));
                    }
                }
            }
            Value::Map(m)
        }
        SetNew => {
            let s: IndexSet<HKey, FxBuildHasher> = IndexSet::default();
            let s = Rc::new(RefCell::new(s));
            if let Some(init) = args.first().filter(|v| !v.is_nullish()) {
                for e in iterate(vm, init)? {
                    s.borrow_mut().insert(HKey(e));
                }
            }
            Value::Set(s)
        }
        MapGet | MapSet | MapHas | MapDelete | MapClear | MapSize | MapForEach | MapKeys | MapValues | MapEntries => {
            let Value::Map(m) = &this else {
                return te(vm, format!("Method Map.prototype.{} called on incompatible receiver", n.name()));
            };
            match n {
                MapGet => m.borrow().get(&HKey(arg(&args, 0))).cloned().unwrap_or(Value::Undef),
                MapSet => {
                    let k = HKey(arg(&args, 0));
                    let v = arg(&args, 1);
                    m.borrow_mut().insert(k, v);
                    this.clone()
                }
                MapHas => Value::Bool(m.borrow().contains_key(&HKey(arg(&args, 0)))),
                MapDelete => Value::Bool(m.borrow_mut().shift_remove(&HKey(arg(&args, 0))).is_some()),
                MapClear => {
                    m.borrow_mut().clear();
                    Value::Undef
                }
                MapSize => Value::Num(m.borrow().len() as f64),
                MapForEach => {
                    let f = arg(&args, 0);
                    let entries: Vec<(Value, Value)> = m.borrow().iter().map(|(k, v)| (k.0.clone(), v.clone())).collect();
                    for (k, v) in entries {
                        vm.call(&f, Value::Undef, vec![v, k, this.clone()])?;
                    }
                    Value::Undef
                }
                MapKeys => Value::Arr(make_array(m.borrow().keys().map(|k| k.0.clone()).collect())),
                MapValues => Value::Arr(make_array(m.borrow().values().cloned().collect())),
                _ => Value::Arr(Array::new_val(
                    m.borrow()
                        .iter()
                        .map(|(k, v)| Value::Arr(Array::new_val(vec![k.0.clone(), v.clone()])))
                        .collect(),
                )),
            }
        }
        SetAdd | SetHas | SetDelete | SetClear | SetSize | SetForEach | SetValues => {
            let Value::Set(s) = &this else {
                return te(vm, format!("Method Set.prototype.{} called on incompatible receiver", n.name()));
            };
            match n {
                SetAdd => {
                    s.borrow_mut().insert(HKey(arg(&args, 0)));
                    this.clone()
                }
                SetHas => Value::Bool(s.borrow().contains(&HKey(arg(&args, 0)))),
                SetDelete => Value::Bool(s.borrow_mut().shift_remove(&HKey(arg(&args, 0)))),
                SetClear => {
                    s.borrow_mut().clear();
                    Value::Undef
                }
                SetSize => Value::Num(s.borrow().len() as f64),
                SetForEach => {
                    let f = arg(&args, 0);
                    let items: Vec<Value> = s.borrow().iter().map(|k| k.0.clone()).collect();
                    for v in items {
                        vm.call(&f, Value::Undef, vec![v.clone(), v, this.clone()])?;
                    }
                    Value::Undef
                }
                _ => Value::Arr(make_array(s.borrow().iter().map(|k| k.0.clone()).collect())),
            }
        }
        DateNow => Value::Num(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_millis() as f64),
        ),
        PerfNow => Value::Num(vm.start.elapsed().as_secs_f64() * 1000.0),
        RegexNew => {
            let (src, flags) = match (arg(&args, 0), arg(&args, 1)) {
                (Value::Regex(r), Value::Undef) => (r.source.to_string(), r.flags.to_string()),
                (Value::Regex(r), f) => (r.source.to_string(), f.to_js_string().s.to_string()),
                (s, Value::Undef) => (if matches!(s, Value::Undef) { String::new() } else { s.to_js_string().s.to_string() }, String::new()),
                (s, f) => (s.to_js_string().s.to_string(), f.to_js_string().s.to_string()),
            };
            match regexp::RegExp::new(&src, &flags) {
                Ok(r) => Value::Regex(Rc::new(r)),
                Err(m) => return Err(vm.syntax_error(m)),
            }
        }
        RegexTest | RegexExec => {
            let Value::Regex(re) = &this else {
                return te(vm, format!("RegExp.prototype.{} called on incompatible receiver", n.name()));
            };
            let s = arg(&args, 0).to_js_string();
            let m = regexp::exec(re, &s);
            match (n, m) {
                (RegexTest, m) => Value::Bool(m.is_some()),
                (_, Some(m)) => regexp::match_array(re, &s, &m),
                (_, None) => Value::Null,
            }
        }
        StrMatch => {
            let s = this_str(&this);
            let re = match arg(&args, 0) {
                Value::Regex(r) => r,
                v => Rc::new(regexp::RegExp::new(&v.to_js_string().s, "").map_err(|m| vm.syntax_error(m))?),
            };
            if re.global {
                let mut out = Vec::new();
                let mut at = 0;
                while let Some(m) = re.match_at(&s.s, at) {
                    let (a, b) = m.groups[0].unwrap();
                    out.push(Value::str(&s.s[a..b]));
                    at = if a == b { b + s.s[b..].chars().next().map_or(1, |c| c.len_utf8()) } else { b };
                }
                re.last_index.set(0);
                if out.is_empty() { Value::Null } else { Value::Arr(Array::new_val(out)) }
            } else {
                match re.match_at(&s.s, 0) {
                    Some(m) => regexp::match_array(&re, &s, &m),
                    None => Value::Null,
                }
            }
        }
        StrMatchAll => {
            let s = this_str(&this);
            let re = match arg(&args, 0) {
                Value::Regex(r) if !r.global => return te(vm, "String.prototype.matchAll called with a non-global RegExp argument".into()),
                Value::Regex(r) => r,
                v => Rc::new(regexp::RegExp::new(&v.to_js_string().s, "g").map_err(|m| vm.syntax_error(m))?),
            };
            let mut out = Vec::new();
            let mut at = 0;
            while let Some(m) = re.match_at(&s.s, at) {
                let (a, b) = m.groups[0].unwrap();
                out.push(regexp::match_array(&re, &s, &m));
                at = if a == b { b + s.s[b..].chars().next().map_or(1, |c| c.len_utf8()) } else { b };
                if at > s.s.len() {
                    break;
                }
            }
            Value::Iter(task::NativeIter::new("RegExp String Iterator", out))
        }
        StrSearch => {
            let s = this_str(&this);
            let re = match arg(&args, 0) {
                Value::Regex(r) => r,
                v => Rc::new(regexp::RegExp::new(&v.to_js_string().s, "").map_err(|m| vm.syntax_error(m))?),
            };
            match re.match_at(&s.s, 0) {
                Some(m) => Value::Num(regexp::utf16_of(&s.s, m.groups[0].unwrap().0) as f64),
                None => Value::Num(-1.0),
            }
        }
        SymbolToString => match &this {
            Value::Symbol(sym) => Value::str(format!("Symbol({})", sym.description.as_deref().unwrap_or(""))),
            _ => Value::Undef,
        },
        TemplateObject => {
            // (n, cooked strings…, raw strings…): the strings array of a tagged template.
            let n = arg(&args, 0).to_number() as usize;
            let cooked = Array::new_val(args[1..1 + n].to_vec());
            let raw = Value::Arr(Array::new_val(args[1 + n..].to_vec()));
            *cooked.props.borrow_mut() = Some(Object::new(Shape::new(vec!["raw".into()]), vec![raw], None));
            Value::Arr(cooked)
        }
        ObjRest => {
            let src = arg(&args, 0);
            let skip: Vec<Rc<JsStr>> = args[1..].iter().map(|k| k.to_js_string()).collect();
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            for k in keys_of(&src) {
                let ks = k.to_js_string();
                if skip.iter().any(|s| s.s == ks.s) {
                    continue;
                }
                vals.push(get_dyn(&src, &k));
                keys.push(Rc::<str>::from(&*ks.s));
            }
            Value::Obj(Object::new(Shape::new(keys), vals, None))
        }
        ObjDefineAccessor => {
            // (object, name, getter, setter): merge with an existing accessor.
            let Value::Obj(o) = arg(&args, 0) else { unreachable!() };
            let name: Rc<str> = (&*arg(&args, 1).to_js_string().s).into();
            let (mut get, mut set) = (arg(&args, 2), arg(&args, 3));
            if let Some(Value::Accessor(a)) = o.get(&name) {
                if matches!(get, Value::Undef) {
                    get = a.get.clone();
                }
                if matches!(set, Value::Undef) {
                    set = a.set.clone();
                }
            }
            o.set(&name, Value::Accessor(Rc::new(task::Accessor { get, set })));
            Value::Undef
        }
        _ => unreachable!(),
    })
}

/// Build an array, choosing numeric storage when every element is a number.
fn regex_split(re: &regexp::RegExp, s: &Rc<JsStr>) -> Vec<Value> {
    let text = &s.s;
    if text.is_empty() {
        return if re.match_at(text, 0).is_some() { vec![] } else { vec![Value::Str(s.clone())] };
    }
    let mut out = Vec::new();
    let (mut last, mut at) = (0usize, 0usize);
    while at < text.len() {
        let Some(m) = re.match_at(text, at) else { break };
        let (ms, me) = m.groups[0].unwrap();
        if me == ms && ms >= text.len() {
            break;
        }
        if me == last || (me == ms && ms == last) {
            at = ms + text[ms..].chars().next().map_or(1, |c| c.len_utf8());
            if me == ms {
                continue;
            }
        }
        out.push(Value::str(&text[last..ms]));
        for g in &m.groups[1..] {
            out.push(g.map_or(Value::Undef, |(a, b)| Value::str(&text[a..b])));
        }
        last = me;
        at = if me == ms { me + text[me..].chars().next().map_or(1, |c| c.len_utf8()) } else { me };
    }
    out.push(Value::str(&text[last..]));
    out
}

fn regex_replace(vm: &mut Vm, re: &Rc<regexp::RegExp>, s: &Rc<JsStr>, rep: &Value) -> NResult {
    let text = &s.s;
    let mut out = String::new();
    let mut last = 0usize;
    let mut at = if re.sticky { regexp::byte_of(text, re.last_index.get()) } else { 0 };
    while let Some(m) = re.match_at(text, at) {
        let (ms, me) = m.groups[0].unwrap();
        out.push_str(&text[last..ms]);
        if crate::vm::is_callable(rep) {
            let mut a: Vec<Value> = m.groups.iter().map(|g| g.map_or(Value::Undef, |(x, y)| Value::str(&text[x..y]))).collect();
            a.push(Value::Num(regexp::utf16_of(text, ms) as f64));
            a.push(Value::Str(s.clone()));
            let r = vm.call(rep, Value::Undef, a)?;
            out.push_str(&r.to_js_string().s);
        } else {
            out.push_str(&regexp::expand(&rep.to_js_string().s, text, &m, &re.names));
        }
        last = me;
        if !re.global {
            break;
        }
        at = if me == ms { me + text[me..].chars().next().map_or(1, |c| c.len_utf8()) } else { me };
        if at > text.len() {
            break;
        }
    }
    if re.global || re.sticky {
        re.last_index.set(0);
    }
    out.push_str(&text[last.min(text.len())..]);
    Ok(Value::str(out))
}

/// `structuredClone`: a deep copy of plain data.
fn deep_clone(v: &Value) -> Value {
    match v {
        Value::Arr(a) => Value::Arr(make_array(a.to_vec().iter().map(deep_clone).collect())),
        Value::Obj(o) if o.class.is_none() => {
            let keys = o.keys();
            let vals = keys.iter().map(|k| deep_clone(&o.get(k).unwrap_or(Value::Undef))).collect();
            Value::Obj(Object::new(Shape::new(keys), vals, None))
        }
        Value::Map(m) => {
            let copy: IndexMap<HKey, Value, rustc_hash::FxBuildHasher> = m.borrow().iter().map(|(k, x)| (HKey(deep_clone(&k.0)), deep_clone(x))).collect();
            Value::Map(Rc::new(std::cell::RefCell::new(copy)))
        }
        Value::Set(s) => {
            let copy: IndexSet<HKey, rustc_hash::FxBuildHasher> = s.borrow().iter().map(|k| HKey(deep_clone(&k.0))).collect();
            Value::Set(Rc::new(std::cell::RefCell::new(copy)))
        }
        v => v.clone(),
    }
}

pub fn make_array(vals: Vec<Value>) -> Rc<Array> {
    if !vals.is_empty() && vals.iter().all(|v| matches!(v, Value::Num(_))) {
        Array::new_num(vals.iter().map(|v| v.to_number()).collect())
    } else {
        Array::new_val(vals)
    }
}

fn replace_contents(a: &Array, vals: Vec<Value>) {
    let mut st = a.store.borrow_mut();
    *st = match &*st {
        ArrStore::Num(_) if vals.iter().all(|v| matches!(v, Value::Num(_))) => {
            ArrStore::Num(vals.iter().map(|v| v.to_number()).collect())
        }
        _ => ArrStore::Val(vals),
    };
}

/// Index key as an array index, if it is one.
pub fn as_index(k: &Value) -> Option<usize> {
    match k {
        Value::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 4294967295.0 => Some(*n as usize),
        Value::Str(s) => match s.s.parse::<u32>() {
            Ok(i) if i.to_string() == *s.s => Some(i as usize),
            _ => None,
        },
        _ => None,
    }
}

/// `obj[key]` for dynamic receivers.
pub fn get_dyn(o: &Value, k: &Value) -> Value {
    match o {
        Value::Arr(a) => {
            if let Some(i) = as_index(k) {
                return a.get(i);
            }
            if let Value::Str(s) = k
                && &*s.s == "length"
            {
                return Value::Num(a.len() as f64);
            }
            if let Some(p) = &*a.props.borrow() {
                return p.get(&k.to_js_string().s).unwrap_or(Value::Undef);
            }
            Value::Undef
        }
        Value::Symbol(sym) if &*k.to_js_string().s == "description" => match &sym.description {
            Some(d) => Value::Str(JsStr::new(&**d)),
            None => Value::Undef,
        },
        Value::Func(f) => match &*f.props.borrow() {
            Some(p) => p.get(&k.to_js_string().s).unwrap_or(Value::Undef),
            None => Value::Undef,
        },
        Value::Regex(r) => match &*k.to_js_string().s {
            "source" => Value::Str(JsStr::new(&*r.source)),
            "flags" => Value::Str(JsStr::new(&*r.flags)),
            "lastIndex" => Value::Num(r.last_index.get() as f64),
            "global" => Value::Bool(r.global),
            "sticky" => Value::Bool(r.sticky),
            "ignoreCase" => Value::Bool(r.flags.contains('i')),
            "multiline" => Value::Bool(r.flags.contains('m')),
            _ => Value::Undef,
        },
        Value::Str(s) => {
            if let Some(i) = as_index(k) {
                return if i < s.len16() { Value::str(s.slice16(i, i + 1)) } else { Value::Undef };
            }
            if let Value::Str(n) = k
                && &*n.s == "length"
            {
                return Value::Num(s.len16() as f64);
            }
            Value::Undef
        }
        Value::Obj(ob) => {
            let key = k.to_js_string();
            ob.get(&key.s).unwrap_or(Value::Undef)
        }
        Value::Map(m) => {
            if let Value::Str(n) = k
                && &*n.s == "size"
            {
                return Value::Num(m.borrow().len() as f64);
            }
            Value::Undef
        }
        Value::Set(s) => {
            if let Value::Str(n) = k
                && &*n.s == "size"
            {
                return Value::Num(s.borrow().len() as f64);
            }
            Value::Undef
        }
        _ => Value::Undef,
    }
}

/// `obj[key] = v` for dynamic receivers. Returns false if not settable.
pub fn set_dyn(o: &Value, k: &Value, v: Value) -> bool {
    match o {
        Value::Func(f) => {
            let key: Rc<str> = (&*k.to_js_string().s).into();
            let mut props = f.props.borrow_mut();
            props.get_or_insert_with(|| Object::new(Shape::new(vec![]), vec![], None)).set(&key, v);
            true
        }
        Value::Regex(r) if &*k.to_js_string().s == "lastIndex" => {
            r.last_index.set(v.to_number().max(0.0) as usize);
            true
        }
        Value::Arr(a) => {
            if let Some(i) = as_index(k) {
                a.set(i, v);
                return true;
            }
            if let Value::Str(s) = k
                && &*s.s == "length"
            {
                a.set_len(v.to_number() as usize);
                return true;
            }
            false
        }
        Value::Obj(ob) => {
            let key: Rc<str> = match k {
                Value::Str(s) => (&*s.s).into(),
                Value::Symbol(sym) => sym.key.clone(),
                k => k.to_display().into(),
            };
            ob.set(&key, v);
            true
        }
        _ => false,
    }
}

/// Look up a built-in method by name for a dynamically typed receiver.
pub fn method_by_name(recv: &Value, name: &str) -> Option<Native> {
    use Native::*;
    let all: &[Native] = match recv {
        Value::Arr(_) => &[
            ArrPush, ArrPop, ArrShift, ArrUnshift, ArrSlice, ArrSplice, ArrConcat, ArrJoin, ArrReverse, ArrIndexOf,
            ArrLastIndexOf, ArrIncludes, ArrMap, ArrFilter, ArrForEach, ArrReduce, ArrReduceRight, ArrFind,
            ArrFindIndex, ArrFindLast, ArrFindLastIndex, ArrSome, ArrEvery, ArrSort, ArrFill, ArrAt, ArrFlat,
            ArrFlatMap, ArrToString, ArrKeys, ArrEntries, ArrToSorted, ArrToReversed, ArrWith,
        ],
        Value::Str(_) => &[
            StrCharAt, StrCharCodeAt, StrCodePointAt, StrIndexOf, StrLastIndexOf, StrIncludes, StrStartsWith,
            StrEndsWith, StrSlice, StrSubstring, StrSubstr, StrToUpper, StrToLower, StrTrim, StrTrimStart,
            StrTrimEnd, StrSplit, StrRepeat, StrPadStart, StrPadEnd, StrReplace, StrReplaceAll, StrConcat, StrAt,
            StrLocaleCompare, StrToString, StrNormalize, StrMatch, StrMatchAll, StrSearch,
        ],
        Value::Num(_) => &[NumToFixed, NumToString, NumToPrecision],
        Value::BigInt(_) => &[NumToString],
        Value::Map(_) => &[MapGet, MapSet, MapHas, MapDelete, MapClear, MapForEach, MapKeys, MapValues, MapEntries],
        Value::Set(_) => &[SetAdd, SetHas, SetDelete, SetClear, SetForEach, SetValues],
        Value::Gen(_) => &[GenNext, GenReturn, GenThrow, IterSelf],
        Value::Iter(_) => &[GenNext, IterSelf],
        Value::Promise(_) => &[PromiseThen, PromiseCatch, PromiseFinally],
        Value::Regex(_) => &[RegexTest, RegexExec],
        Value::Symbol(_) => &[SymbolToString],
        Value::Func(_) | Value::Native(_) | Value::Bound(_) => &[FunctionBind, FunctionCall, FunctionApply],
        _ => &[],
    };
    all.iter().copied().find(|n| n.name() == name).or(match (recv, name) {
        (Value::Set(_), "keys") => Some(SetValues),
        (Value::Bool(_), "toString") => Some(StrToString),
        _ => None,
    })
}
