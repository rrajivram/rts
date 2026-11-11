//! `rts --dump`: print a program's instructions in readable form.
//!
//! Slots are shown by the name of the variable they hold at that point
//! (`n`, `total`), as `#2` for a slot preloaded with the constant 2, and as
//! `num3` / `bool3` / `val3` for temporaries (number, boolean or value slot 3).
//! Fields of class instances are shown by name.
//! Jump targets are labels (`L12`), and indexes into the function's tables
//! (constants, call sites, properties, classes, globals) are replaced by what
//! they refer to.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::bytecode::*;
use crate::runtime::{Value, num_to_string};
use crate::types::Kind;

/// The whole program: the top-level code first, then each function. Functions
/// of the built-in prelude (`Error` and friends) are left out unless `prelude`.
pub fn dump(prog: &Program, prelude: bool) -> String {
    let mut out = String::new();
    let main = prog.main as usize;
    let order = std::iter::once(main).chain((0..prog.funcs.len()).filter(|&i| i != main));
    for i in order {
        let f = &prog.funcs[i];
        if f.prelude && !prelude {
            continue;
        }
        dump_function(prog, f, &mut out);
        out.push('\n');
    }
    out
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Num => "number",
        Kind::Bool => "boolean",
        Kind::Big => "bigint",
        Kind::Val => "value",
    }
}

fn dump_function(prog: &Program, f: &FunctionCode, out: &mut String) {
    let params: Vec<String> = f
        .param_names
        .iter()
        .zip(&f.params)
        .enumerate()
        .map(|(i, (n, k))| {
            let dots = if f.rest && i + 1 == f.params.len() { "..." } else { "" };
            format!("{dots}{n}: {}", kind_name(*k))
        })
        .collect();
    let _ = writeln!(out, "function {}({})", f.dump_name, params.join(", "));
    let _ = writeln!(
        out,
        "  ; slots: {} number/boolean, {} value{}",
        f.scalar_slots,
        f.value_slots,
        if f.capture_names.is_empty() { String::new() } else { format!("; captures: {}", f.capture_names.join(", ")) }
    );
    let labels: BTreeSet<u32> = f.code.iter().filter_map(jump_target).collect();
    let d = Dumper { prog, f };
    for (pc, ins) in f.code.iter().enumerate() {
        if labels.contains(&(pc as u32)) {
            let _ = writeln!(out, "L{pc}:");
        }
        let _ = writeln!(out, "    {}", d.instruction(pc as u32, ins));
    }
}

fn jump_target(ins: &Instruction) -> Option<u32> {
    match *ins {
        Instruction::Jump { to }
        | Instruction::JumpIf { to, .. }
        | Instruction::JumpIfCompare { to, .. }
        | Instruction::JumpIfTruthy { to, .. }
        | Instruction::JumpIfNullish { to, .. } => Some(to),
        Instruction::TryBegin { catch, .. } => Some(catch),
        _ => None,
    }
}

struct Dumper<'a> {
    prog: &'a Program,
    f: &'a FunctionCode,
}

impl Dumper<'_> {
    /// What scalar (`kind` Num/Bool) or value slot `index` holds at `pc`.
    fn slot(&self, kind: Kind, index: u16, pc: u32) -> String {
        let file = if kind == Kind::Val { Kind::Val } else { Kind::Num };
        let named = self
            .f
            .slot_names
            .iter()
            .rev()
            .find(|n| n.kind == file && n.index == index && n.from <= pc)
            .and_then(|n| n.name.clone());
        match named {
            Some(n) => n.to_string(),
            None => match kind {
                Kind::Num => format!("num{index}"),
                Kind::Bool => format!("bool{index}"),
                Kind::Big => format!("big{index}"),
                Kind::Val => format!("val{index}"),
            },
        }
    }

    fn constant(v: &Value) -> String {
        match v {
            Value::Str(s) => format!("{:?}", &*s.s),
            Value::Num(n) => num_to_string(*n),
            Value::BigInt(n) => format!("{n}n"),
            other => other.to_display(),
        }
    }

    fn global(&self, variant: &str, g: usize) -> String {
        let names = if variant.contains("Number") || variant.contains("Bool") || variant.contains("BigInt") {
            &self.prog.global_scalar_names
        } else {
            &self.prog.global_value_names
        };
        names.get(g).map_or(format!("global{g}"), |n| n.to_string())
    }

    fn instruction(&self, pc: u32, ins: &Instruction) -> String {
        if let Instruction::Call { site } = ins {
            return self.call(pc, &self.f.call_sites[*site as usize]);
        }
        // Start from the derived Debug form (`Name { field: value, .. }`) and
        // replace each field's value with what it refers to.
        let text = format!("{ins:?}");
        let Some(open) = text.find(" { ") else { return text };
        let variant = &text[..open];
        let body = &text[open + 3..text.len() - 2];
        let fields: Vec<String> = split_fields(body)
            .into_iter()
            .map(|(field, value)| format!("{field}: {}", self.field(variant, field, value, pc)))
            .collect();
        format!("{variant} {{ {} }}", fields.join(", "))
    }

    fn field(&self, variant: &str, field: &str, value: &str, pc: u32) -> String {
        let num = || value.parse::<usize>().unwrap_or(0);
        for (prefix, kind) in [("NumSlot(", Kind::Num), ("BoolSlot(", Kind::Bool), ("BigSlot(", Kind::Big), ("ValSlot(", Kind::Val)] {
            if let Some(rest) = value.strip_prefix(prefix) {
                let index = rest.trim_end_matches(')').parse().unwrap_or(0);
                return self.slot(kind, index, pc);
            }
        }
        match (variant, field) {
            (_, "to" | "catch") => format!("L{value}"),
            ("LoadConstant" | "LoadBigInt", "constant") => Self::constant(&self.f.constants[num()]),
            (_, "constant") | (_, "value") => match value.parse::<f64>() {
                Ok(n) => num_to_string(n),
                Err(_) => value.to_string(),
            },
            (_, "property") => format!("{:?}", &*self.f.property_sites[num()].name),
            ("MakeClosure", "function") => self.prog.funcs[num()].dump_name.to_string(),
            ("MakeClosure", "captures") => {
                let names: Vec<String> = self.f.capture_lists[num()]
                    .iter()
                    .map(|c| match c {
                        CaptureFrom::Slot(s) => self.slot(Kind::Val, s.0, pc),
                        CaptureFrom::Capture(i) => self.f.capture_names[*i as usize].to_string(),
                    })
                    .collect();
                format!("[{}]", names.join(", "))
            }
            (_, "field") => match self.f.field_names.iter().find(|(at, _)| *at == pc) {
                Some((_, n)) => n.to_string(),
                None => format!("#{value}"),
            },
            (_, "class") => self.prog.classes[num()].name.to_string(),
            (_, "shape") => format!("{{{}}}", self.prog.shapes[num()].keys.join(", ")),
            (_, "global") => self.global(variant, num()),
            (_, "capture") => self.f.capture_names[num()].to_string(),
            _ => value.to_string(),
        }
    }

    fn call(&self, pc: u32, site: &CallSite) -> String {
        let function = match &site.target {
            CallTarget::Direct(f) => self.prog.funcs[*f as usize].dump_name.to_string(),
            CallTarget::Value(s) => self.slot(Kind::Val, s.0, pc),
            CallTarget::Method { class, slot } => match self.prog.classes.get(*class as usize) {
                Some(c) => self.prog.funcs[c.vtable[*slot as usize] as usize].dump_name.to_string(),
                None => format!("method #{slot}"),
            },
            CallTarget::Property(p) => format!(".{}", self.f.property_sites[*p as usize].name),
            CallTarget::Native(n) => format!("{n:?} (built-in)"),
        };
        let args: Vec<String> = site
            .args
            .iter()
            .zip(&site.spread)
            .map(|((k, r), spread)| format!("{}{}", if *spread { "..." } else { "" }, self.slot(*k, *r, pc)))
            .collect();
        let mut s = format!("Call {{ function: {function}");
        if let Some(t) = site.this {
            let _ = write!(s, ", this: {}", self.slot(Kind::Val, t.0, pc));
        }
        let _ = write!(s, ", args: [{}]", args.join(", "));
        if let Some((k, r)) = site.output {
            let _ = write!(s, ", output: {}", self.slot(k, r, pc));
        }
        s.push_str(" }");
        s
    }
}

/// Split `a: x, b: Y(1), c: z` into field/value pairs.
fn split_fields(body: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let bytes = body.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&body[start..i]);
                start = i + 2;
            }
            _ => {}
        }
    }
    out.push(&body[start..]);
    out.into_iter()
        .filter_map(|f| f.split_once(": "))
        .collect()
}
