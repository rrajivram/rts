//! Semantic types and their mapping to storage kinds.

use std::fmt;
use std::rc::Rc;

use crate::ast::Name;

pub type ClassId = u32;
pub type EnumId = u32;
pub type TParamId = u32;

/// Where a value of some type lives at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// f64 in a number slot (see `bytecode.rs`).
    Num,
    /// 0/1 in a boolean slot.
    Bool,
    /// An `i128` BigInt in two consecutive number slots (low half first).
    Big,
    /// Tagged, refcounted `Value` in a value slot.
    Val,
}

impl Kind {
    pub fn is_scalar(self) -> bool {
        !matches!(self, Kind::Val)
    }

    /// How many slots a value of this kind takes.
    pub fn width(self) -> u16 {
        if self == Kind::Big { 2 } else { 1 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FParam {
    pub name: Name,
    pub ty: Type,
    /// May be omitted by the caller (optional or has a default).
    pub optional: bool,
    pub rest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncType {
    pub tparams: Vec<TParamId>,
    pub params: Vec<FParam>,
    pub ret: Type,
}

impl FuncType {
    pub fn min_args(&self) -> usize {
        self.params.iter().filter(|p| !p.optional && !p.rest).count()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub name: Name,
    pub ty: Type,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjType {
    pub props: Vec<Prop>,
    /// `[key: string]: T`
    pub index: Option<Type>,
    /// Call signature.
    pub call: Option<Rc<FuncType>>,
}

impl ObjType {
    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| &*p.name == name)
    }
}

/// Built-in namespaces and constructors usable as values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinNs {
    Math,
    Console,
    Json,
    Object,
    Number,
    String,
    Array,
    Boolean,
    MapCtor,
    SetCtor,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Number,
    BigInt,
    Boolean,
    String,
    Void,
    Undefined,
    Null,
    Any,
    Unknown,
    Never,
    StrLit(Name),
    BoolLit(bool),
    NumLit(f64),
    Array(Rc<Type>),
    Tuple(Rc<[Type]>),
    Func(Rc<FuncType>),
    Object(Rc<ObjType>),
    /// Instance of a class, with type arguments.
    Class(ClassId, Rc<[Type]>),
    /// The class constructor value itself.
    ClassCtor(ClassId),
    /// A named interface (structural, possibly recursive), with type arguments.
    Iface(u32, Rc<[Type]>),
    Enum(EnumId),
    EnumObj(EnumId),
    Union(Rc<[Type]>),
    Param(TParamId),
    Map(Rc<Type>, Rc<Type>),
    Set(Rc<Type>),
    Builtin(BuiltinNs),
}

impl Type {
    pub fn kind(&self) -> Kind {
        match self {
            Type::Number | Type::NumLit(_) | Type::Enum(_) => Kind::Num,
            Type::Boolean | Type::BoolLit(_) => Kind::Bool,
            Type::BigInt => Kind::Big,
            Type::Union(ts) => {
                if ts.iter().all(|t| matches!(t.kind(), Kind::Num)) {
                    Kind::Num
                } else if ts.iter().all(|t| matches!(t.kind(), Kind::Bool)) {
                    Kind::Bool
                } else if ts.iter().all(|t| matches!(t.kind(), Kind::Big)) {
                    Kind::Big
                } else {
                    Kind::Val
                }
            }
            _ => Kind::Val,
        }
    }

    pub fn array(t: Type) -> Type {
        Type::Array(Rc::new(t))
    }

    pub fn func(params: Vec<FParam>, ret: Type) -> Type {
        Type::Func(Rc::new(FuncType {
            tparams: vec![],
            params,
            ret,
        }))
    }

    /// Normalizing union constructor.
    pub fn union(types: impl IntoIterator<Item = Type>) -> Type {
        let mut out: Vec<Type> = Vec::new();
        fn add(out: &mut Vec<Type>, t: Type) {
            match t {
                Type::Union(ts) => {
                    for t in ts.iter() {
                        add(out, t.clone());
                    }
                }
                Type::Never => {}
                t => {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
        }
        for t in types {
            add(&mut out, t);
        }
        if out.iter().any(|t| matches!(t, Type::Any)) {
            return Type::Any;
        }
        if out.iter().any(|t| matches!(t, Type::Unknown)) {
            return Type::Unknown;
        }
        // Literals absorbed by their base type.
        let has = |out: &Vec<Type>, t: &Type| out.contains(t);
        if has(&out, &Type::String) {
            out.retain(|t| !matches!(t, Type::StrLit(_)));
        }
        if has(&out, &Type::Number) {
            out.retain(|t| !matches!(t, Type::NumLit(_)));
        }
        if has(&out, &Type::BoolLit(true)) && has(&out, &Type::BoolLit(false)) {
            out.retain(|t| !matches!(t, Type::BoolLit(_)));
            out.push(Type::Boolean);
        }
        if has(&out, &Type::Boolean) {
            out.retain(|t| !matches!(t, Type::BoolLit(_)));
        }
        if has(&out, &Type::Void) {
            out.retain(|t| !matches!(t, Type::Undefined));
        }
        match out.len() {
            0 => Type::Never,
            1 => out.pop().unwrap(),
            _ => Type::Union(out.into()),
        }
    }

    pub fn members(&self) -> Vec<Type> {
        match self {
            Type::Union(ts) => ts.to_vec(),
            Type::Boolean => vec![Type::BoolLit(true), Type::BoolLit(false)],
            t => vec![t.clone()],
        }
    }

    /// Widen literal types (for `let` declarations and mutable positions).
    pub fn widen(&self) -> Type {
        match self {
            Type::StrLit(_) => Type::String,
            Type::NumLit(_) => Type::Number,
            Type::BoolLit(_) => Type::Boolean,
            Type::Union(ts) => Type::union(ts.iter().map(|t| t.widen())),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| t.widen()).collect()),
            t => t.clone(),
        }
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Type::Null | Type::Undefined | Type::Void)
    }

    pub fn can_be_nullish(&self) -> bool {
        match self {
            Type::Any | Type::Unknown | Type::Param(_) => true,
            Type::Union(ts) => ts.iter().any(|t| t.can_be_nullish()),
            t => t.is_nullish(),
        }
    }

    /// Remove `null`/`undefined` from a type.
    pub fn non_null(&self) -> Type {
        match self {
            Type::Union(ts) => Type::union(ts.iter().filter(|t| !t.is_nullish()).cloned()),
            t if t.is_nullish() => Type::Never,
            t => t.clone(),
        }
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self, Type::Any | Type::Unknown)
    }

    pub fn is_numeric(&self) -> bool {
        self.kind() == Kind::Num
    }

    pub fn is_stringy(&self) -> bool {
        match self {
            Type::String | Type::StrLit(_) => true,
            Type::Union(ts) => ts.iter().all(|t| t.is_stringy()),
            _ => false,
        }
    }

    pub fn is_booleanish(&self) -> bool {
        self.kind() == Kind::Bool
    }

    pub fn is_bigint(&self) -> bool {
        self.kind() == Kind::Big
    }
}

/// Display needs class/enum names; the checker supplies them.
pub trait TypeNames {
    fn class_name(&self, id: ClassId) -> String;
    fn enum_name(&self, id: EnumId) -> String;
    fn tparam_name(&self, id: TParamId) -> String;
    fn iface_name(&self, id: u32) -> String;
}

pub struct Disp<'a, N: TypeNames>(pub &'a Type, pub &'a N);

impl<N: TypeNames> fmt::Display for Disp<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.1;
        let d = |t: &Type| format!("{}", Disp(t, n));
        match self.0 {
            Type::Number => write!(f, "number"),
            Type::BigInt => write!(f, "bigint"),
            Type::Boolean => write!(f, "boolean"),
            Type::String => write!(f, "string"),
            Type::Void => write!(f, "void"),
            Type::Undefined => write!(f, "undefined"),
            Type::Null => write!(f, "null"),
            Type::Any => write!(f, "any"),
            Type::Unknown => write!(f, "unknown"),
            Type::Never => write!(f, "never"),
            Type::StrLit(s) => write!(f, "\"{s}\""),
            Type::BoolLit(b) => write!(f, "{b}"),
            Type::NumLit(v) => write!(f, "{}", crate::runtime::num_to_string(*v)),
            Type::Array(t) => {
                if matches!(**t, Type::Union(_) | Type::Func(_)) {
                    write!(f, "({})[]", d(t))
                } else {
                    write!(f, "{}[]", d(t))
                }
            }
            Type::Tuple(ts) => write!(f, "[{}]", ts.iter().map(d).collect::<Vec<_>>().join(", ")),
            Type::Func(ft) => {
                let ps: Vec<String> = ft
                    .params
                    .iter()
                    .map(|p| {
                        format!(
                            "{}{}{}: {}",
                            if p.rest { "..." } else { "" },
                            p.name,
                            if p.optional { "?" } else { "" },
                            d(&p.ty)
                        )
                    })
                    .collect();
                write!(f, "({}) => {}", ps.join(", "), d(&ft.ret))
            }
            Type::Object(o) => {
                let mut ps: Vec<String> = o
                    .props
                    .iter()
                    .map(|p| format!("{}{}: {}", p.name, if p.optional { "?" } else { "" }, d(&p.ty)))
                    .collect();
                if let Some(t) = &o.index {
                    ps.push(format!("[key: string]: {}", d(t)));
                }
                write!(f, "{{ {} }}", ps.join("; "))
            }
            Type::Class(id, args) => {
                write!(f, "{}", n.class_name(*id))?;
                if !args.is_empty() {
                    write!(f, "<{}>", args.iter().map(d).collect::<Vec<_>>().join(", "))?;
                }
                Ok(())
            }
            Type::ClassCtor(id) => write!(f, "typeof {}", n.class_name(*id)),
            Type::Iface(id, args) => {
                write!(f, "{}", n.iface_name(*id))?;
                if !args.is_empty() {
                    write!(f, "<{}>", args.iter().map(d).collect::<Vec<_>>().join(", "))?;
                }
                Ok(())
            }
            Type::Enum(id) => write!(f, "{}", n.enum_name(*id)),
            Type::EnumObj(id) => write!(f, "typeof {}", n.enum_name(*id)),
            Type::Union(ts) => write!(f, "{}", ts.iter().map(d).collect::<Vec<_>>().join(" | ")),
            Type::Param(id) => write!(f, "{}", n.tparam_name(*id)),
            Type::Map(k, v) => write!(f, "Map<{}, {}>", d(k), d(v)),
            Type::Set(t) => write!(f, "Set<{}>", d(t)),
            Type::Builtin(b) => write!(f, "{b:?}"),
        }
    }
}
