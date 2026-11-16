//! Regular expressions: JS syntax translated to `fancy-regex` (which has
//! backreferences and lookaround). Positions are UTF-16 indices at the JS
//! boundary and byte offsets inside.

use std::cell::Cell;
use std::rc::Rc;

use super::*;

#[derive(Debug)]
pub struct RegExp {
    pub source: Rc<str>,
    pub flags: Rc<str>,
    pub re: fancy_regex::Regex,
    /// `lastIndex`, in UTF-16 units (used with the `g` and `y` flags).
    pub last_index: Cell<usize>,
    pub global: bool,
    pub sticky: bool,
    /// Capture group names, by group number (index 0 is the whole match).
    pub names: Vec<Option<Rc<str>>>,
}

/// One match: byte ranges of the whole match and each group.
pub struct Match {
    pub groups: Vec<Option<(usize, usize)>>,
}

impl RegExp {
    pub fn new(source: &str, flags: &str) -> Result<RegExp, String> {
        for f in flags.chars() {
            if !"dgimsuyv".contains(f) {
                return Err(format!("Invalid flags supplied to RegExp constructor '{flags}'"));
            }
        }
        let mut pat = String::new();
        for (f, inline) in [('i', "i"), ('m', "m"), ('s', "s")] {
            if flags.contains(f) {
                pat.push_str(&format!("(?{inline})"));
            }
        }
        pat.push_str(&translate(source));
        let re = fancy_regex::Regex::new(&pat).map_err(|e| format!("Invalid regular expression: /{source}/{flags}: {e}"))?;
        let names = re.capture_names().map(|n| n.map(Rc::from)).collect();
        Ok(RegExp {
            source: source.into(),
            flags: flags.into(),
            re,
            last_index: Cell::new(0),
            global: flags.contains('g'),
            sticky: flags.contains('y'),
            names,
        })
    }

    /// Match starting at byte offset `at`.
    pub fn match_at(&self, s: &str, at: usize) -> Option<Match> {
        if at > s.len() {
            return None;
        }
        let caps = self.re.captures_from_pos(s, at).ok()??;
        let whole = caps.get(0)?;
        if self.sticky && whole.start() != at {
            return None;
        }
        Some(Match {
            groups: (0..caps.len()).map(|i| caps.get(i).map(|m| (m.start(), m.end()))).collect(),
        })
    }

    pub fn display(&self) -> String {
        format!("/{}/{}", if self.source.is_empty() { "(?:)" } else { &self.source }, self.flags)
    }
}

/// JS regex syntax → Rust regex syntax.
fn translate(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 8);
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' if i + 1 < cs.len() => {
                let n = cs[i + 1];
                i += 2;
                match n {
                    // JS classes are ASCII; Rust's are Unicode.
                    'd' => out.push_str(if in_class { "0-9" } else { "[0-9]" }),
                    'D' => out.push_str(if in_class { "\\D" } else { "[^0-9]" }),
                    'w' => out.push_str(if in_class { "A-Za-z0-9_" } else { "[A-Za-z0-9_]" }),
                    'W' => out.push_str(if in_class { "\\W" } else { "[^A-Za-z0-9_]" }),
                    '/' => out.push('/'),
                    'u' if i < cs.len() && cs[i] == '{' => {
                        let end = cs[i..].iter().position(|&c| c == '}').map_or(cs.len(), |p| i + p);
                        let hex: String = cs[i + 1..end].iter().collect();
                        out.push_str(&format!("\\x{{{hex}}}"));
                        i = end + 1;
                    }
                    'u' if i + 4 <= cs.len() => {
                        let hex: String = cs[i..i + 4].iter().collect();
                        out.push_str(&format!("\\x{{{hex}}}"));
                        i += 4;
                    }
                    'c' if i < cs.len() => {
                        out.push_str(&format!("\\x{{{:x}}}", (cs[i] as u32) % 32));
                        i += 1;
                    }
                    '0' => out.push_str("\\x{0}"),
                    '-' => out.push_str("\\-"),
                    n => {
                        out.push('\\');
                        out.push(n);
                    }
                }
                continue;
            }
            '[' if !in_class => {
                if cs.get(i + 1) == Some(&'^') && cs.get(i + 2) == Some(&']') {
                    out.push_str("(?s:.)");
                    i += 3;
                    continue;
                }
                if cs.get(i + 1) == Some(&']') {
                    out.push_str("(?!)");
                    i += 2;
                    continue;
                }
                in_class = true;
                out.push('[');
                // A literal `[` right after `[` or `[^` must be escaped in Rust.
                if cs.get(i + 1) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
            }
            '[' if in_class => out.push_str("\\["),
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            '&' | '~' if in_class => {
                out.push('\\');
                out.push(c);
            }
            // A quantifier is copied whole; any other `{` or `}` is a literal in JS.
            '{' if !in_class && is_quantifier(&cs[i..]) => {
                let end = i + cs[i..].iter().position(|&c| c == '}').unwrap();
                out.extend(&cs[i..=end]);
                i = end + 1;
                continue;
            }
            '{' | '}' if !in_class => {
                out.push('\\');
                out.push(c);
            }
            // Named groups: `(?<name>` (but not lookbehind `(?<=`, `(?<!`).
            '(' if !in_class && cs.get(i + 1) == Some(&'?') && cs.get(i + 2) == Some(&'<') && !matches!(cs.get(i + 3), Some('=') | Some('!')) => {
                out.push_str("(?P<");
                i += 3;
                continue;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn is_quantifier(rest: &[char]) -> bool {
    let end = match rest.iter().position(|&c| c == '}') {
        Some(e) => e,
        None => return false,
    };
    let body: String = rest[1..end].iter().collect();
    let mut parts = body.splitn(2, ',');
    let a = parts.next().unwrap_or("");
    !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && parts.next().is_none_or(|b| b.chars().all(|c| c.is_ascii_digit()))
}


/// Byte offset of UTF-16 index `i` in `s`.
pub fn byte_of(s: &str, i: usize) -> usize {
    if s.is_ascii() {
        return i.min(s.len());
    }
    let mut u = 0;
    for (b, c) in s.char_indices() {
        if u >= i {
            return b;
        }
        u += c.len_utf16();
    }
    s.len()
}

/// UTF-16 index of byte offset `b` in `s`.
pub fn utf16_of(s: &str, b: usize) -> usize {
    if s.is_ascii() {
        return b;
    }
    s[..b].encode_utf16().count()
}

/// `re.exec(s)`, honoring and updating `lastIndex` for `g`/`y`.
pub fn exec(re: &RegExp, s: &Gc<JsStr>) -> Option<Match> {
    let from = if re.global || re.sticky { byte_of(&s.s, re.last_index.get()) } else { 0 };
    if (re.global || re.sticky) && re.last_index.get() > s.len16() {
        re.last_index.set(0);
        return None;
    }
    match re.match_at(&s.s, from) {
        Some(m) => {
            if re.global || re.sticky {
                let (st, en) = m.groups[0].unwrap();
                let next = if en == st { en + s.s[en..].chars().next().map_or(1, |c| c.len_utf8()) } else { en };
                re.last_index.set(utf16_of(&s.s, next.min(s.s.len())) + usize::from(next > s.s.len()));
            }
            Some(m)
        }
        None => {
            if re.global || re.sticky {
                re.last_index.set(0);
            }
            None
        }
    }
}

/// The array `exec` and `match` return: the match and groups, with `index`,
/// `input` and `groups` properties.
pub fn match_array(re: &RegExp, s: &Gc<JsStr>, m: &Match) -> Value {
    let items: Vec<Value> = m.groups.iter().map(|g| g.map_or(Value::Undef, |(a, b)| Value::str(&s.s[a..b]))).collect();
    let arr = Array::new_val(items);
    let index = utf16_of(&s.s, m.groups[0].unwrap().0);
    let groups = if re.names.iter().any(|n| n.is_some()) {
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        for (i, n) in re.names.iter().enumerate() {
            if let Some(n) = n {
                keys.push(n.clone());
                vals.push(m.groups[i].map_or(Value::Undef, |(a, b)| Value::str(&s.s[a..b])));
            }
        }
        Value::Obj(Object::new(Shape::new_null_proto(keys), vals, None))
    } else {
        Value::Undef
    };
    let props = Object::new(
        Shape::new(vec!["index".into(), "input".into(), "groups".into()]),
        vec![Value::Num(index as f64), Value::Str(s.clone()), groups],
        None,
    );
    *arr.props.borrow_mut() = Some(props);
    Value::Arr(arr)
}

/// Expand `$1`, `$&`, `$<name>`, `` $` ``, `$'` and `$$` in a replacement string.
pub fn expand(rep: &str, s: &str, m: &Match, names: &[Option<Rc<str>>]) -> String {
    let mut out = String::new();
    let cs: Vec<char> = rep.chars().collect();
    let (ms, me) = m.groups[0].unwrap();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '$' && i + 1 < cs.len() {
            match cs[i + 1] {
                '$' => {
                    out.push('$');
                    i += 2;
                    continue;
                }
                '&' => {
                    out.push_str(&s[ms..me]);
                    i += 2;
                    continue;
                }
                '`' => {
                    out.push_str(&s[..ms]);
                    i += 2;
                    continue;
                }
                '\'' => {
                    out.push_str(&s[me..]);
                    i += 2;
                    continue;
                }
                '<' => {
                    if let Some(end) = cs[i..].iter().position(|&c| c == '>') {
                        let name: String = cs[i + 2..i + end].iter().collect();
                        if let Some(g) = names.iter().position(|n| n.as_deref() == Some(name.as_str())) {
                            if let Some((a, b)) = m.groups[g] {
                                out.push_str(&s[a..b]);
                            }
                            i += end + 1;
                            continue;
                        }
                    }
                }
                d if d.is_ascii_digit() => {
                    // Two-digit group numbers win when that group exists.
                    let two = cs.get(i + 2).filter(|c| c.is_ascii_digit()).map(|c| (d as usize - 48) * 10 + (*c as usize - 48));
                    let (g, len) = match two {
                        Some(g) if g < m.groups.len() && g > 0 => (g, 3),
                        _ => (d as usize - 48, 2),
                    };
                    if g > 0 && g < m.groups.len() {
                        if let Some((a, b)) = m.groups[g] {
                            out.push_str(&s[a..b]);
                        }
                        i += len;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}
