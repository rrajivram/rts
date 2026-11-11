//! Types of built-in namespaces, functions and methods.

use std::rc::Rc;

use super::{Checker, Scope, VB};
use crate::hir::{HExpr, HK};
use crate::runtime::Native;
use crate::types::*;

pub fn install_globals(module: &Rc<Scope>) {
    let mut v = module.vals.borrow_mut();
    for (n, ns) in [
        ("Math", BuiltinNs::Math),
        ("console", BuiltinNs::Console),
        ("JSON", BuiltinNs::Json),
        ("Object", BuiltinNs::Object),
        ("Number", BuiltinNs::Number),
        ("String", BuiltinNs::String),
        ("Array", BuiltinNs::Array),
        ("Boolean", BuiltinNs::Boolean),
        ("Map", BuiltinNs::MapCtor),
        ("Set", BuiltinNs::SetCtor),
        // Without a tracing collector there is nothing for weak references to
        // add: a WeakMap is a Map.
        ("WeakMap", BuiltinNs::MapCtor),
        ("WeakSet", BuiltinNs::SetCtor),
        ("Promise", BuiltinNs::Promise),
        ("Symbol", BuiltinNs::Symbol),
        ("RegExp", BuiltinNs::RegExp),
    ] {
        v.insert(n.into(), VB::Builtin(ns));
    }
    for (n, f) in [
        ("parseInt", Native::ParseInt),
        ("parseFloat", Native::ParseFloat),
        ("isNaN", Native::IsNaN),
        ("isFinite", Native::IsFinite),
        ("BigInt", Native::BigIntFn),
        ("setTimeout", Native::SetTimeout),
        ("setInterval", Native::SetInterval),
        ("setImmediate", Native::SetImmediate),
        ("clearTimeout", Native::ClearTimeout),
        ("clearInterval", Native::ClearTimeout),
        ("clearImmediate", Native::ClearTimeout),
        ("queueMicrotask", Native::QueueMicrotask),
        ("\u{1}templateObject", Native::TemplateObject),
        ("structuredClone", Native::StructuredClone),
        ("encodeURIComponent", Native::EncodeUriComponent),
        ("encodeURI", Native::EncodeUri),
        ("decodeURIComponent", Native::DecodeUriComponent),
        ("decodeURI", Native::DecodeUri),
        // Used by the prelude and src/node/*.ts.
        ("__rts_now", Native::DateNowMs),
        ("__rts_date_offset", Native::DateOffset),
        ("__rts_date_zone_name", Native::DateZoneName),
        ("__rts_date_parse", Native::DateParse),
        ("__rts_deep_equal", Native::DeepEqual),
        ("__rts_format", Native::UtilFormat),
        ("__rts_inspect", Native::UtilInspect),
        ("__rts_cwd", Native::ProcessCwd),
        ("__rts_os_info", Native::OsInfo),
        ("__rts_fs_read", Native::FsReadFile),
        ("__rts_fs_write", Native::FsWriteFile),
        ("__rts_fs_append", Native::FsAppendFile),
        ("__rts_fs_exists", Native::FsExists),
        ("__rts_fs_mkdir", Native::FsMkdir),
        ("__rts_fs_readdir", Native::FsReaddir),
        ("__rts_fs_stat", Native::FsStat),
        ("__rts_fs_unlink", Native::FsUnlink),
        ("__rts_fs_rm", Native::FsRm),
        ("__rts_fs_rename", Native::FsRename),
        ("__rts_fs_copy", Native::FsCopy),
        ("__rts_fs_realpath", Native::FsRealpath),
    ] {
        v.insert(n.into(), VB::NativeFn(f));
    }
}

fn p(name: &str, ty: Type) -> FParam {
    FParam {
        name: name.into(),
        ty,
        optional: false,
        rest: false,
    }
}
fn opt(name: &str, ty: Type) -> FParam {
    FParam {
        name: name.into(),
        ty: Type::union([ty, Type::Undefined]),
        optional: true,
        rest: false,
    }
}
fn rest(name: &str, elem: Type) -> FParam {
    FParam {
        name: name.into(),
        ty: Type::array(elem),
        optional: false,
        rest: true,
    }
}
fn ft(params: Vec<FParam>, ret: Type) -> Rc<FuncType> {
    Rc::new(FuncType {
        tparams: vec![],
        params,
        ret,
    })
}
fn gft(tps: Vec<TParamId>, params: Vec<FParam>, ret: Type) -> Rc<FuncType> {
    Rc::new(FuncType {
        tparams: tps,
        params,
        ret,
    })
}
fn num() -> Type {
    Type::Number
}
fn string() -> Type {
    Type::String
}
fn boolean() -> Type {
    Type::Boolean
}
fn any() -> Type {
    Type::Any
}

pub enum NsMember {
    Const(HExpr),
    Method(Native, Rc<FuncType>),
}

/// Type of a global built-in function used as a value or called directly.
pub fn global_fn_type(n: Native) -> Rc<FuncType> {
    match n {
        Native::ParseInt => ft(vec![p("s", any()), opt("radix", num())], num()),
        Native::ParseFloat => ft(vec![p("s", any())], num()),
        Native::IsNaN | Native::IsFinite => ft(vec![p("x", any())], boolean()),
        Native::StringFn => ft(vec![opt("x", any())], string()),
        Native::NumberFn => ft(vec![opt("x", any())], num()),
        Native::BooleanFn => ft(vec![opt("x", any())], boolean()),
        Native::BigIntFn => ft(vec![p("x", any())], Type::BigInt),
        Native::DateNowMs | Native::DateOffset | Native::DateParse => ft(vec![rest("args", any())], num()),
        Native::DateZoneName | Native::UtilFormat | Native::UtilInspect | Native::ProcessCwd | Native::FsReadFile | Native::FsRealpath => {
            ft(vec![rest("args", any())], string())
        }
        Native::EncodeUriComponent | Native::EncodeUri | Native::DecodeUriComponent | Native::DecodeUri => ft(vec![p("s", any())], string()),
        Native::DeepEqual | Native::FsExists => ft(vec![rest("args", any())], boolean()),
        Native::FsReaddir => ft(vec![rest("args", any())], Type::array(string())),
        Native::SetTimeout | Native::SetInterval => ft(vec![p("callback", any()), opt("ms", num()), rest("args", any())], num()),
        Native::SetImmediate => ft(vec![p("callback", any()), rest("args", any())], num()),
        Native::ClearTimeout => ft(vec![opt("id", any())], Type::Void),
        Native::QueueMicrotask => ft(vec![p("callback", Type::func(vec![], Type::Void))], Type::Void),
        Native::StructuredClone => {
            let t = Type::Param(0);
            let _ = t;
            ft(vec![p("value", any())], any())
        }
        _ => ft(vec![rest("args", any())], any()),
    }
}

impl Checker {
    pub fn ns_member(&mut self, ns: BuiltinNs, name: &str) -> Option<NsMember> {
        use Native::*;
        let t = Type::Param(self.tp_t);
        let tt = self.tp_t;
        match ns {
            BuiltinNs::Promise => {
                let any_p = || Type::promise(any());
                return Some(match name {
                    "resolve" => NsMember::Method(PromiseResolve, gft(vec![tt], vec![opt("value", t.clone())], Type::promise(t))),
                    "reject" => NsMember::Method(PromiseReject, ft(vec![opt("reason", any())], Type::promise(Type::Never))),
                    // Result types are refined from the argument (see `promise_combinator_type`).
                    "all" => NsMember::Method(PromiseAll, ft(vec![p("values", any())], any_p())),
                    "allSettled" => NsMember::Method(PromiseAllSettled, ft(vec![p("values", any())], any_p())),
                    "race" => NsMember::Method(PromiseRace, ft(vec![p("values", any())], any_p())),
                    "any" => NsMember::Method(PromiseAny, ft(vec![p("values", any())], any_p())),
                    "withResolvers" => NsMember::Method(PromiseWithResolvers, ft(vec![], any())),
                    _ => return None,
                });
            }
            BuiltinNs::Symbol => {
                return match name {
                    "for" => Some(NsMember::Method(SymbolFor, ft(vec![p("key", string())], Type::Symbol))),
                    "iterator" | "asyncIterator" | "hasInstance" | "toPrimitive" | "toStringTag" | "species" => Some(NsMember::Const(
                        self.native_call(SymbolWellKnown, None, vec![crate::hir::HElem::Expr(HExpr::str(name))], Type::Symbol),
                    )),
                    _ => None,
                };
            }
            _ => {}
        }
        let c = |v: f64| Some(NsMember::Const(HExpr::num(v)));
        let m = |n: Native, f: Rc<FuncType>| Some(NsMember::Method(n, f));
        let n1 = || ft(vec![p("x", num())], num());
        let n2 = || ft(vec![p("x", num()), p("y", num())], num());
        match ns {
            BuiltinNs::Math => match name {
                "PI" => c(std::f64::consts::PI),
                "E" => c(std::f64::consts::E),
                "LN2" => c(std::f64::consts::LN_2),
                "LN10" => c(std::f64::consts::LN_10),
                "LOG2E" => c(std::f64::consts::LOG2_E),
                "LOG10E" => c(std::f64::consts::LOG10_E),
                "SQRT2" => c(std::f64::consts::SQRT_2),
                "SQRT1_2" => c(std::f64::consts::FRAC_1_SQRT_2),
                "floor" => m(MathFloor, n1()),
                "ceil" => m(MathCeil, n1()),
                "round" => m(MathRound, n1()),
                "trunc" => m(MathTrunc, n1()),
                "sqrt" => m(MathSqrt, n1()),
                "abs" => m(MathAbs, n1()),
                "sign" => m(MathSign, n1()),
                "sin" => m(MathSin, n1()),
                "cos" => m(MathCos, n1()),
                "tan" => m(MathTan, n1()),
                "asin" => m(MathAsin, n1()),
                "acos" => m(MathAcos, n1()),
                "atan" => m(MathAtan, n1()),
                "exp" => m(MathExp, n1()),
                "log" => m(MathLog, n1()),
                "log2" => m(MathLog2, n1()),
                "log10" => m(MathLog10, n1()),
                "cbrt" => m(MathCbrt, n1()),
                "fround" => m(MathFround, n1()),
                "clz32" => m(MathClz32, n1()),
                "atan2" => m(MathAtan2, n2()),
                "pow" => m(MathPow, n2()),
                "imul" => m(MathImul, n2()),
                "min" => m(MathMin, ft(vec![rest("values", num())], num())),
                "max" => m(MathMax, ft(vec![rest("values", num())], num())),
                "hypot" => m(MathHypot, ft(vec![rest("values", num())], num())),
                "random" => m(MathRandom, ft(vec![], num())),
                _ => None,
            },
            BuiltinNs::Console => match name {
                "log" | "info" | "debug" => m(ConsoleLog, ft(vec![rest("args", any())], Type::Void)),
                "error" => m(ConsoleError, ft(vec![rest("args", any())], Type::Void)),
                "warn" => m(ConsoleWarn, ft(vec![rest("args", any())], Type::Void)),
                _ => None,
            },
            BuiltinNs::Json => match name {
                "stringify" => m(
                    JsonStringify,
                    ft(vec![p("value", any()), opt("replacer", any()), opt("space", any())], string()),
                ),
                "parse" => m(JsonParse, ft(vec![p("text", string())], any())),
                _ => None,
            },
            BuiltinNs::Object => match name {
                "is" => m(ObjectIs, ft(vec![p("a", any()), p("b", any())], boolean())),
                "keys" => m(ObjKeys, ft(vec![p("o", any())], Type::array(string()))),
                "values" => m(ObjValues, ft(vec![p("o", any())], Type::array(any()))),
                "entries" => m(
                    ObjEntries,
                    ft(vec![p("o", any())], Type::array(Type::Tuple(vec![string(), any()].into()))),
                ),
                "assign" => m(ObjAssign, ft(vec![p("target", any()), rest("sources", any())], any())),
                "freeze" => m(ObjFreeze, gft(vec![self.tp_t], vec![p("o", Type::Param(self.tp_t))], Type::Param(self.tp_t))),
                "fromEntries" => m(ObjFromEntries, ft(vec![p("entries", any())], any())),
                _ => None,
            },
            BuiltinNs::Number => match name {
                "MAX_SAFE_INTEGER" => c(9007199254740991.0),
                "MIN_SAFE_INTEGER" => c(-9007199254740991.0),
                "EPSILON" => c(f64::EPSILON),
                "MAX_VALUE" => c(f64::MAX),
                "MIN_VALUE" => c(5e-324),
                "POSITIVE_INFINITY" => c(f64::INFINITY),
                "NEGATIVE_INFINITY" => c(f64::NEG_INFINITY),
                "NaN" => c(f64::NAN),
                "isInteger" => m(NumIsInteger, ft(vec![p("x", any())], boolean())),
                "isFinite" => m(NumIsFinite, ft(vec![p("x", any())], boolean())),
                "isNaN" => m(NumIsNaN, ft(vec![p("x", any())], boolean())),
                "isSafeInteger" => m(NumIsSafeInteger, ft(vec![p("x", any())], boolean())),
                "parseFloat" => m(ParseFloat, global_fn_type(ParseFloat)),
                "parseInt" => m(ParseInt, global_fn_type(ParseInt)),
                _ => None,
            },
            BuiltinNs::String => match name {
                "fromCharCode" => m(StrFromCharCode, ft(vec![rest("codes", num())], string())),
                _ => None,
            },
            BuiltinNs::Array => match name {
                "isArray" => m(ArrIsArray, ft(vec![p("x", any())], boolean())),
                "of" => m(ArrOf, gft(vec![self.tp_t], vec![rest("items", Type::Param(self.tp_t))], Type::array(Type::Param(self.tp_t)))),
                "from" => m(ArrFrom, ft(vec![p("src", any()), opt("map", any())], Type::array(any()))),
                _ => None,
            },
            _ => None,
        }
    }

    /// Built-in method on a value of a built-in type. Signatures are already
    /// specialized to the receiver's element types; `U` is a free type parameter.
    pub fn builtin_method(&mut self, recv: &Type, name: &str) -> Option<(Native, Rc<FuncType>)> {
        use Native::*;
        let u = Type::Param(self.tp_u);
        let tu = self.tp_u;
        let r = |n: Native, f: Rc<FuncType>| Some((n, f));
        match recv {
            Type::Array(_) | Type::Tuple(_) => {
                let t = match recv {
                    Type::Array(t) => (**t).clone(),
                    Type::Tuple(ts) => Type::union(ts.iter().cloned()),
                    _ => unreachable!(),
                };
                let arr = Type::array(t.clone());
                let cb = |ret: Type| {
                    Type::Func(ft(
                        vec![p("value", t.clone()), p("index", num()), p("array", arr.clone())],
                        ret,
                    ))
                };
                let opt_t = Type::union([t.clone(), Type::Undefined]);
                match name {
                    "push" => r(ArrPush, ft(vec![rest("items", t.clone())], num())),
                    "unshift" => r(ArrUnshift, ft(vec![rest("items", t.clone())], num())),
                    "pop" => r(ArrPop, ft(vec![], opt_t)),
                    "shift" => r(ArrShift, ft(vec![], opt_t)),
                    "slice" => r(ArrSlice, ft(vec![opt("start", num()), opt("end", num())], arr.clone())),
                    "splice" => r(
                        ArrSplice,
                        ft(vec![p("start", num()), opt("deleteCount", num()), rest("items", t.clone())], arr.clone()),
                    ),
                    "concat" => r(ArrConcat, ft(vec![rest("items", any())], arr.clone())),
                    "join" => r(ArrJoin, ft(vec![opt("separator", string())], string())),
                    "reverse" => r(ArrReverse, ft(vec![], arr.clone())),
                    "indexOf" => r(ArrIndexOf, ft(vec![p("x", t.clone()), opt("from", num())], num())),
                    "lastIndexOf" => r(ArrLastIndexOf, ft(vec![p("x", t.clone()), opt("from", num())], num())),
                    "includes" => r(ArrIncludes, ft(vec![p("x", t.clone()), opt("from", num())], boolean())),
                    "map" => r(ArrMap, gft(vec![tu], vec![p("f", cb(u.clone()))], Type::array(u.clone()))),
                    "flatMap" => r(ArrFlatMap, gft(vec![tu], vec![p("f", cb(u.clone()))], Type::array(any()))),
                    "filter" => r(ArrFilter, ft(vec![p("f", cb(any()))], arr.clone())),
                    "forEach" => r(ArrForEach, ft(vec![p("f", cb(Type::Void))], Type::Void)),
                    "find" => r(ArrFind, ft(vec![p("f", cb(any()))], Type::union([t.clone(), Type::Undefined]))),
                    "findLast" => r(ArrFindLast, ft(vec![p("f", cb(any()))], Type::union([t.clone(), Type::Undefined]))),
                    "findIndex" => r(ArrFindIndex, ft(vec![p("f", cb(any()))], num())),
                    "findLastIndex" => r(ArrFindLastIndex, ft(vec![p("f", cb(any()))], num())),
                    "some" => r(ArrSome, ft(vec![p("f", cb(any()))], boolean())),
                    "every" => r(ArrEvery, ft(vec![p("f", cb(any()))], boolean())),
                    "sort" => r(
                        ArrSort,
                        ft(vec![opt("compare", Type::Func(ft(vec![p("a", t.clone()), p("b", t.clone())], num())))], arr.clone()),
                    ),
                    "fill" => r(ArrFill, ft(vec![p("value", t.clone()), opt("start", num()), opt("end", num())], arr.clone())),
                    "at" => r(ArrAt, ft(vec![p("index", num())], Type::union([t.clone(), Type::Undefined]))),
                    "flat" => {
                        let inner = match &t {
                            Type::Array(e) => (**e).clone(),
                            t => t.clone(),
                        };
                        r(ArrFlat, ft(vec![opt("depth", num())], Type::array(inner)))
                    }
                    "keys" => r(ArrKeys, ft(vec![], Type::array(num()))),
                    "entries" => r(ArrEntries, ft(vec![], Type::array(Type::Tuple(vec![num(), t.clone()].into())))),
                    "toString" => r(ArrToString, ft(vec![], string())),
                    "toSorted" => r(ArrToSorted, ft(vec![opt("compare", Type::Func(ft(vec![p("a", t.clone()), p("b", t.clone())], num())))], arr.clone())),
                    "toReversed" => r(ArrToReversed, ft(vec![], arr.clone())),
                    "with" => r(ArrWith, ft(vec![p("index", num()), p("value", t.clone())], arr.clone())),
                    // reduce is special-cased by the caller (two overloads).
                    "reduce" | "reduceRight" => {
                        let n = if name == "reduce" { ArrReduce } else { ArrReduceRight };
                        r(
                            n,
                            gft(
                                vec![tu],
                                vec![
                                    p(
                                        "f",
                                        Type::Func(ft(
                                            vec![p("acc", u.clone()), p("value", t.clone()), p("index", num()), p("array", arr.clone())],
                                            u.clone(),
                                        )),
                                    ),
                                    p("initial", u.clone()),
                                ],
                                u.clone(),
                            ),
                        )
                    }
                    _ => None,
                }
            }
            Type::String | Type::StrLit(_) => match name {
                "charAt" => r(StrCharAt, ft(vec![opt("i", num())], string())),
                "charCodeAt" => r(StrCharCodeAt, ft(vec![opt("i", num())], num())),
                "codePointAt" => r(StrCodePointAt, ft(vec![p("i", num())], Type::union([num(), Type::Undefined]))),
                "indexOf" => r(StrIndexOf, ft(vec![p("s", string()), opt("from", num())], num())),
                "lastIndexOf" => r(StrLastIndexOf, ft(vec![p("s", string()), opt("from", num())], num())),
                "includes" => r(StrIncludes, ft(vec![p("s", string()), opt("from", num())], boolean())),
                "startsWith" => r(StrStartsWith, ft(vec![p("s", string()), opt("pos", num())], boolean())),
                "endsWith" => r(StrEndsWith, ft(vec![p("s", string()), opt("end", num())], boolean())),
                "slice" => r(StrSlice, ft(vec![opt("start", num()), opt("end", num())], string())),
                "substring" => r(StrSubstring, ft(vec![p("start", num()), opt("end", num())], string())),
                "substr" => r(StrSubstr, ft(vec![p("start", num()), opt("len", num())], string())),
                "toUpperCase" | "toLocaleUpperCase" => r(StrToUpper, ft(vec![], string())),
                "toLowerCase" | "toLocaleLowerCase" => r(StrToLower, ft(vec![], string())),
                "trim" => r(StrTrim, ft(vec![], string())),
                "trimStart" => r(StrTrimStart, ft(vec![], string())),
                "trimEnd" => r(StrTrimEnd, ft(vec![], string())),
                "split" => r(StrSplit, ft(vec![opt("sep", any()), opt("limit", num())], Type::array(string()))),
                "match" => r(StrMatch, ft(vec![p("pattern", any())], any())),
                "matchAll" => r(StrMatchAll, ft(vec![p("pattern", any())], Type::iter(IterKind::IterableIterator, false, any(), any(), any()))),
                "search" => r(StrSearch, ft(vec![p("pattern", any())], num())),
                "repeat" => r(StrRepeat, ft(vec![p("n", num())], string())),
                "padStart" => r(StrPadStart, ft(vec![p("n", num()), opt("fill", string())], string())),
                "padEnd" => r(StrPadEnd, ft(vec![p("n", num()), opt("fill", string())], string())),
                "replace" => r(StrReplace, ft(vec![p("pattern", any()), p("replacement", any())], string())),
                "replaceAll" => r(StrReplaceAll, ft(vec![p("pattern", any()), p("replacement", any())], string())),
                "concat" => r(StrConcat, ft(vec![rest("parts", any())], string())),
                "at" => r(StrAt, ft(vec![p("i", num())], Type::union([string(), Type::Undefined]))),
                "localeCompare" => r(StrLocaleCompare, ft(vec![p("other", string())], num())),
                "toString" | "valueOf" => r(StrToString, ft(vec![], string())),
                "normalize" => r(StrNormalize, ft(vec![opt("form", string())], string())),
                _ => None,
            },
            Type::Number | Type::NumLit(_) | Type::Enum(_) => match name {
                "toFixed" => r(NumToFixed, ft(vec![opt("digits", num())], string())),
                "toString" => r(NumToString, ft(vec![opt("radix", num())], string())),
                "toPrecision" => r(NumToPrecision, ft(vec![opt("p", num())], string())),
                _ => None,
            },
            Type::BigInt => match name {
                "toString" => r(NumToString, ft(vec![opt("radix", num())], string())),
                _ => None,
            },
            Type::Symbol => match name {
                "toString" => r(SymbolToString, ft(vec![], string())),
                _ => None,
            },
            Type::RegExp => match name {
                "test" => r(RegexTest, ft(vec![p("s", string())], boolean())),
                "exec" => r(RegexExec, ft(vec![p("s", string())], any())),
                "toString" => r(StrToString, ft(vec![], string())),
                _ => None,
            },
            Type::Promise(t) => {
                let t = (**t).clone();
                let cb = |param: Type| Type::Func(ft(vec![p("value", param)], u.clone()));
                match name {
                    "then" => r(
                        PromiseThen,
                        gft(vec![tu], vec![opt("onfulfilled", cb(t.clone())), opt("onrejected", cb(any()))], Type::promise(u.clone())),
                    ),
                    "catch" => r(PromiseCatch, gft(vec![tu], vec![opt("onrejected", cb(any()))], Type::promise(Type::union([t, u.clone()])))),
                    "finally" => r(PromiseFinally, ft(vec![opt("onfinally", Type::func(vec![], any()))], recv.clone())),
                    _ => None,
                }
            }
            Type::Iter(it) => {
                let res = super::iter_result_type(it.yield_ty.clone(), it.return_ty.clone());
                let res = if it.is_async { Type::promise(res) } else { res };
                match name {
                    "next" => r(GenNext, ft(vec![opt("value", it.next_ty.clone())], res)),
                    "return" => r(GenReturn, ft(vec![opt("value", any())], res)),
                    "throw" => r(GenThrow, ft(vec![opt("e", any())], res)),
                    "\u{2}Symbol.iterator" | "\u{2}Symbol.asyncIterator" => r(IterSelf, ft(vec![], recv.clone())),
                    _ => None,
                }
            }
            Type::Func(f) => match name {
                "bind" => r(FunctionBind, ft(vec![opt("thisArg", any()), rest("args", any())], recv.clone())),
                "call" => r(FunctionCall, ft(vec![opt("thisArg", any()), rest("args", any())], f.ret.clone())),
                "apply" => r(FunctionApply, ft(vec![opt("thisArg", any()), opt("args", any())], f.ret.clone())),
                _ => None,
            },
            Type::Boolean | Type::BoolLit(_) => match name {
                "toString" => r(StrToString, ft(vec![], string())),
                _ => None,
            },
            Type::Map(k, v) => {
                let (k, v) = ((**k).clone(), (**v).clone());
                let me = recv.clone();
                match name {
                    "get" => r(MapGet, ft(vec![p("key", k)], Type::union([v, Type::Undefined]))),
                    "set" => r(MapSet, ft(vec![p("key", k), p("value", v)], me)),
                    "has" => r(MapHas, ft(vec![p("key", k)], boolean())),
                    "delete" => r(MapDelete, ft(vec![p("key", k)], boolean())),
                    "clear" => r(MapClear, ft(vec![], Type::Void)),
                    "forEach" => r(
                        MapForEach,
                        ft(vec![p("f", Type::Func(ft(vec![p("value", v), p("key", k), p("map", me)], Type::Void)))], Type::Void),
                    ),
                    "keys" => r(MapKeys, ft(vec![], Type::array(k))),
                    "values" => r(MapValues, ft(vec![], Type::array(v))),
                    "entries" => r(MapEntries, ft(vec![], Type::array(Type::Tuple(vec![k, v].into())))),
                    _ => None,
                }
            }
            Type::Set(t) => {
                let t = (**t).clone();
                let me = recv.clone();
                match name {
                    "add" => r(SetAdd, ft(vec![p("value", t)], me)),
                    "has" => r(SetHas, ft(vec![p("value", t)], boolean())),
                    "delete" => r(SetDelete, ft(vec![p("value", t)], boolean())),
                    "clear" => r(SetClear, ft(vec![], Type::Void)),
                    "forEach" => r(
                        SetForEach,
                        ft(vec![p("f", Type::Func(ft(vec![p("value", t.clone()), p("value2", t), p("set", me)], Type::Void)))], Type::Void),
                    ),
                    "values" | "keys" => r(SetValues, ft(vec![], Type::array(t))),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The element type visited by `for-of` / spread.
    pub fn iter_elem(&mut self, t: &Type) -> Option<Type> {
        Some(match t {
            Type::Array(e) => (**e).clone(),
            Type::Tuple(ts) => Type::union(ts.iter().cloned()),
            Type::String | Type::StrLit(_) => Type::String,
            Type::Map(k, v) => Type::Tuple(vec![(**k).clone(), (**v).clone()].into()),
            Type::Set(e) => (**e).clone(),
            Type::Any => Type::Any,
            Type::Iter(it) if !it.is_async => it.yield_ty.clone(),
            Type::Iter(_) => return None,
            Type::Union(ts) => {
                let mut out = Vec::new();
                for m in ts.iter() {
                    if m.is_nullish() {
                        continue;
                    }
                    out.push(self.iter_elem(m)?);
                }
                Type::union(out)
            }
            t => {
                // An object with a [Symbol.iterator]() method.
                let o = self.obj_view(t)?;
                let f = o.prop("\u{2}Symbol.iterator")?.ty.clone();
                let Type::Func(ft) = f else { return Some(Type::Any) };
                match &ft.ret {
                    Type::Iter(it) => it.yield_ty.clone(),
                    r => self.iterator_value_type(&r.clone()).unwrap_or(Type::Any),
                }
            }
        })
    }

    /// Element type of `for await` over an object with [Symbol.asyncIterator]().
    pub fn async_iter_elem(&mut self, t: &Type) -> Option<Type> {
        match t {
            Type::Any => Some(Type::Any),
            Type::Iter(it) if it.is_async => Some(it.yield_ty.clone()),
            t => {
                let o = self.obj_view(t)?;
                let f = o.prop("\u{2}Symbol.asyncIterator")?.ty.clone();
                let Type::Func(ft) = f else { return Some(Type::Any) };
                Some(match &ft.ret {
                    Type::Iter(it) => it.yield_ty.clone(),
                    r => self.iterator_value_type(&r.clone()).map(|t| t.awaited()).unwrap_or(Type::Any),
                })
            }
        }
    }

    /// The `value` type of what an iterator object's `next()` returns.
    pub fn iterator_value_type(&mut self, it: &Type) -> Option<Type> {
        let o = self.obj_view(it)?;
        let Type::Func(next) = o.prop("next")?.ty.clone() else { return None };
        let ret = next.ret.awaited();
        let mut out = Vec::new();
        for m in ret.members() {
            if let Some(r) = self.obj_view(&m) {
                let done_true = r.prop("done").is_some_and(|d| d.ty == Type::BoolLit(true));
                if !done_true && let Some(v) = r.prop("value") {
                    out.push(v.ty.clone());
                }
            }
        }
        Some(Type::union(out))
    }
}

impl Checker {
    /// `Promise.all([a, b])`: a promise of the awaited element types.
    pub fn promise_combinator_type(&mut self, n: Native, arg: Option<&Type>) -> Type {
        let Some(t) = arg else { return Type::promise(Type::Any) };
        let elems = match t {
            Type::Tuple(ts) => Some(ts.iter().map(|t| t.awaited()).collect::<Vec<_>>()),
            _ => None,
        };
        let elem = self.iter_elem(t).unwrap_or(Type::Any).awaited();
        Type::promise(match n {
            Native::PromiseAll => match elems {
                Some(ts) => Type::Tuple(ts.into()),
                None => Type::array(elem),
            },
            Native::PromiseRace | Native::PromiseAny => elem,
            _ => Type::array(Type::Any),
        })
    }
}

/// Dates/performance/process are reached via member access on unknown globals.
pub fn special_global_call(obj: &str, prop: &str) -> Option<(Native, Rc<FuncType>)> {
    match (obj, prop) {
        ("Date", "now") => Some((Native::DateNow, ft(vec![], Type::Number))),
        ("performance", "now") => Some((Native::PerfNow, ft(vec![], Type::Number))),
        _ => None,
    }
}

#[allow(dead_code)]
fn _unused(_: HK) {}
