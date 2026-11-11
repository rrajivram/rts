//! Tokenizer. Produces a flat token vector; template literals become a single
//! token whose `${…}` holes are tokenized recursively.

use std::rc::Rc;

use crate::error::{CompileError, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    /// `123n`
    BigInt(i128),
    Str(Rc<str>),
    /// `` `a${x}b` `` → quasis ["a", "b"], exprs [[x]]. quasis.len() == exprs.len() + 1.
    Template {
        quasis: Vec<Rc<str>>,
        exprs: Vec<Vec<Token>>,
        /// The quasis as written, escapes unprocessed (`String.raw`).
        raws: Vec<Rc<str>>,
    },
    Ident(Rc<str>),
    /// `/pattern/flags`
    Regex { pattern: Rc<str>, flags: Rc<str> },
    /// Punctuator / operator, e.g. "(", "=>", ">>>=".
    P(&'static str),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// A line terminator precedes this token (for automatic semicolon insertion).
    pub nl_before: bool,
}

/// Longest first, so the scanner can take the first match.
const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", "=", ".", "@", "#",
];

pub fn tokenize(src: &str) -> Result<Vec<Token>, CompileError> {
    tokenize_at(src, 0)
}

/// Tokenize a file whose first byte is at offset `base` of the program's
/// combined source space (see `error::Sources`), so spans identify the file.
pub fn tokenize_at(src: &str, base: usize) -> Result<Vec<Token>, CompileError> {
    let mut lx = Lexer {
        src: src.as_bytes(),
        text: src,
        pos: 0,
        base,
    };
    let mut toks = lx.run(false)?;
    toks.push(Token {
        tok: Tok::Eof,
        span: Span::new(base + src.len(), base + src.len()),
        nl_before: true,
    });
    Ok(toks)
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    /// Offset of `src` within the original file (for nested template lexing).
    base: usize,
}

impl<'a> Lexer<'a> {
    fn err<T>(&self, msg: impl Into<String>, at: usize) -> Result<T, CompileError> {
        Err(CompileError::new(msg, Span::new(self.base + at, self.base + at + 1)))
    }

    fn peek(&self, off: usize) -> u8 {
        *self.src.get(self.pos + off).unwrap_or(&0)
    }

    /// Tokenize until end of input, or (when `in_template`) until the `}` that
    /// closes a `${` hole at brace depth 0.
    fn run(&mut self, in_template: bool) -> Result<Vec<Token>, CompileError> {
        let mut out = Vec::new();
        let mut nl = false;
        let mut depth = 0usize;
        loop {
            // Skip whitespace and comments.
            loop {
                let c = self.peek(0);
                if c == b'\n' {
                    nl = true;
                    self.pos += 1;
                } else if c == b' ' || c == b'\t' || c == b'\r' {
                    self.pos += 1;
                } else if c == b'/' && self.peek(1) == b'/' {
                    while self.pos < self.src.len() && self.peek(0) != b'\n' {
                        self.pos += 1;
                    }
                } else if c == b'/' && self.peek(1) == b'*' {
                    let start = self.pos;
                    self.pos += 2;
                    loop {
                        if self.pos >= self.src.len() {
                            return self.err("unterminated comment", start);
                        }
                        if self.peek(0) == b'*' && self.peek(1) == b'/' {
                            self.pos += 2;
                            break;
                        }
                        if self.peek(0) == b'\n' {
                            nl = true;
                        }
                        self.pos += 1;
                    }
                } else if c >= 0x80 {
                    // Unicode whitespace (e.g. NBSP, BOM) — skip; identifiers are ASCII-only for now.
                    let ch = self.text[self.pos..].chars().next().unwrap();
                    if ch.is_whitespace() || ch == '\u{feff}' {
                        if ch == '\u{2028}' || ch == '\u{2029}' {
                            nl = true;
                        }
                        self.pos += ch.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            if self.pos >= self.src.len() {
                if in_template {
                    return self.err("unterminated template literal", self.pos);
                }
                return Ok(out);
            }
            let start = self.pos;
            let c = self.peek(0);
            let tok = if c.is_ascii_digit() || (c == b'.' && self.peek(1).is_ascii_digit()) {
                self.number()?
            } else if c == b'"' || c == b'\'' {
                Tok::Str(self.string(c)?.into())
            } else if c == b'`' {
                self.template()?
            } else if c == b'/' && regex_allowed(out.last().map(|t: &Token| &t.tok)) {
                self.regex()?
            } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
                while self.pos < self.src.len() {
                    let c = self.peek(0);
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                        self.pos += 1;
                    } else if c >= 0x80 {
                        let ch = self.text[self.pos..].chars().next().unwrap();
                        if ch.is_alphanumeric() {
                            self.pos += ch.len_utf8();
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                if self.pos == start {
                    return self.err("unexpected character", start);
                }
                Tok::Ident(self.text[start..self.pos].into())
            } else {
                if in_template {
                    if c == b'{' {
                        depth += 1;
                    } else if c == b'}' {
                        if depth == 0 {
                            self.pos += 1;
                            return Ok(out);
                        }
                        depth -= 1;
                    }
                }
                let rest = &self.text[self.pos..];
                match PUNCTS.iter().find(|p| rest.starts_with(**p)) {
                    // `?.5` is `? .5`, not optional chaining.
                    Some(&"?.") if self.peek(2).is_ascii_digit() => {
                        self.pos += 1;
                        Tok::P("?")
                    }
                    Some(p) => {
                        self.pos += p.len();
                        Tok::P(p)
                    }
                    None => return self.err(format!("unexpected character '{}'", c as char), start),
                }
            };
            out.push(Token {
                tok,
                span: Span::new(self.base + start, self.base + self.pos),
                nl_before: nl,
            });
            nl = false;
        }
    }

    /// `/pattern/flags`; the opening `/` is at `pos`.
    fn regex(&mut self) -> Result<Tok, CompileError> {
        let start = self.pos;
        self.pos += 1;
        let mut in_class = false;
        loop {
            match self.peek(0) {
                0 | b'\n' => return self.err("unterminated regular expression", start),
                b'\\' => self.pos += 2,
                b'[' => {
                    in_class = true;
                    self.pos += 1;
                }
                b']' => {
                    in_class = false;
                    self.pos += 1;
                }
                b'/' if !in_class => break,
                _ => self.pos += 1,
            }
        }
        let pattern = &self.text[start + 1..self.pos];
        self.pos += 1;
        let fstart = self.pos;
        while self.peek(0).is_ascii_alphabetic() {
            self.pos += 1;
        }
        Ok(Tok::Regex {
            pattern: pattern.into(),
            flags: self.text[fstart..self.pos].into(),
        })
    }

    fn number(&mut self) -> Result<Tok, CompileError> {
        let start = self.pos;
        if self.peek(0) == b'0' && matches!(self.peek(1) | 0x20, b'x' | b'o' | b'b') {
            let radix = match self.peek(1) | 0x20 {
                b'x' => 16,
                b'o' => 8,
                _ => 2,
            };
            self.pos += 2;
            let ds = self.pos;
            while self.peek(0).is_ascii_alphanumeric() || self.peek(0) == b'_' {
                self.pos += 1;
            }
            let mut digits: String = self.text[ds..self.pos].chars().filter(|c| *c != '_').collect();
            if digits.ends_with('n') {
                digits.pop();
                return match i128::from_str_radix(&digits, radix) {
                    Ok(v) => Ok(Tok::BigInt(v)),
                    Err(_) => self.err("BigInt literal is too large (BigInts are 128-bit)", start),
                };
            }
            return match u64::from_str_radix(&digits, radix) {
                Ok(v) => Ok(Tok::Num(v as f64)),
                Err(_) => self.err("invalid number literal", start),
            };
        }
        while self.peek(0).is_ascii_digit() || self.peek(0) == b'_' {
            self.pos += 1;
        }
        if self.peek(0) == b'.' {
            self.pos += 1;
            while self.peek(0).is_ascii_digit() || self.peek(0) == b'_' {
                self.pos += 1;
            }
        }
        if self.peek(0) | 0x20 == b'e' {
            let save = self.pos;
            self.pos += 1;
            if self.peek(0) == b'+' || self.peek(0) == b'-' {
                self.pos += 1;
            }
            if !self.peek(0).is_ascii_digit() {
                self.pos = save;
            }
            while self.peek(0).is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.peek(0) == b'n' {
            let text: String = self.text[start..self.pos].chars().filter(|c| *c != '_').collect();
            self.pos += 1;
            return match text.parse::<i128>() {
                Ok(v) => Ok(Tok::BigInt(v)),
                Err(_) if text.bytes().all(|b| b.is_ascii_digit()) => self.err("BigInt literal is too large (BigInts are 128-bit)", start),
                Err(_) => self.err("invalid BigInt literal", start),
            };
        }
        let text: String = self.text[start..self.pos].chars().filter(|c| *c != '_').collect();
        match text.parse::<f64>() {
            Ok(v) => Ok(Tok::Num(v)),
            Err(_) => self.err("invalid number literal", start),
        }
    }

    /// Reads one escape sequence after the backslash; appends to `out`.
    fn escape(&mut self, out: &mut String) -> Result<(), CompileError> {
        let at = self.pos;
        let c = self.peek(0);
        self.pos += 1;
        match c {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'v' => out.push('\u{b}'),
            b'0' if !self.peek(0).is_ascii_digit() => out.push('\0'),
            b'\n' => {}
            b'\r' => {
                if self.peek(0) == b'\n' {
                    self.pos += 1;
                }
            }
            b'x' => {
                let h = self.text.get(self.pos..self.pos + 2).unwrap_or("");
                let v = u32::from_str_radix(h, 16).or_else(|_| self.err("bad \\x escape", at))?;
                self.pos += 2;
                out.push(char::from_u32(v).unwrap());
            }
            b'u' => {
                let v = if self.peek(0) == b'{' {
                    let end = self.text[self.pos..].find('}').map(|i| self.pos + i);
                    let Some(end) = end else {
                        return self.err("bad \\u escape", at);
                    };
                    let v = u32::from_str_radix(&self.text[self.pos + 1..end], 16)
                        .or_else(|_| self.err("bad \\u escape", at))?;
                    self.pos = end + 1;
                    v
                } else {
                    let h = self.text.get(self.pos..self.pos + 4).unwrap_or("");
                    let v = u32::from_str_radix(h, 16).or_else(|_| self.err("bad \\u escape", at))?;
                    self.pos += 4;
                    v
                };
                // Lone surrogates can't be represented in a Rust string.
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            _ => {
                // Any other char escapes to itself (including multi-byte).
                self.pos -= 1;
                let ch = self.text[self.pos..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                out.push(ch);
            }
        }
        Ok(())
    }

    fn string(&mut self, q: u8) -> Result<String, CompileError> {
        let start = self.pos;
        self.pos += 1;
        let mut s = String::new();
        loop {
            if self.pos >= self.src.len() || self.peek(0) == b'\n' {
                return self.err("unterminated string", start);
            }
            let c = self.peek(0);
            if c == q {
                self.pos += 1;
                return Ok(s);
            }
            if c == b'\\' {
                self.pos += 1;
                self.escape(&mut s)?;
                continue;
            }
            let ch = self.text[self.pos..].chars().next().unwrap();
            s.push(ch);
            self.pos += ch.len_utf8();
        }
    }

    fn template(&mut self) -> Result<Tok, CompileError> {
        let start = self.pos;
        self.pos += 1;
        let mut quasis = Vec::new();
        let mut exprs = Vec::new();
        let mut raws = Vec::new();
        let mut raw_start = self.pos;
        let mut cur = String::new();
        loop {
            if self.pos >= self.src.len() {
                return self.err("unterminated template literal", start);
            }
            let c = self.peek(0);
            if c == b'`' {
                raws.push(self.text[raw_start..self.pos].into());
                self.pos += 1;
                quasis.push(cur.into());
                return Ok(Tok::Template { quasis, exprs, raws });
            }
            if c == b'\\' {
                self.pos += 1;
                self.escape(&mut cur)?;
                continue;
            }
            if c == b'$' && self.peek(1) == b'{' {
                raws.push(self.text[raw_start..self.pos].into());
                self.pos += 2;
                quasis.push(std::mem::take(&mut cur).into());
                let mut sub = Lexer {
                    src: self.src,
                    text: self.text,
                    pos: self.pos,
                    base: self.base,
                };
                let mut toks = sub.run(true)?;
                let end = self.base + sub.pos;
                toks.push(Token {
                    tok: Tok::Eof,
                    span: Span::new(end, end),
                    nl_before: true,
                });
                self.pos = sub.pos;
                raw_start = self.pos;
                exprs.push(toks);
                continue;
            }
            let ch = self.text[self.pos..].chars().next().unwrap();
            cur.push(ch);
            self.pos += ch.len_utf8();
        }
    }
}

/// Whether a `/` after this token starts a regular expression (rather than
/// being division).
fn regex_allowed(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        Some(Tok::P(p)) => !matches!(*p, ")" | "]" | "}" | "++" | "--"),
        Some(Tok::Ident(k)) => matches!(
            &**k,
            "return" | "typeof" | "case" | "do" | "else" | "in" | "instanceof" | "new" | "delete" | "void" | "throw" | "yield" | "await" | "of"
        ),
        _ => false,
    }
}
