//! Hand-written recursive-descent parser with Pratt-style binary operators.
//!
//! Ambiguities that TypeScript resolves by lookahead (arrow functions vs.
//! parenthesized expressions, `f<T>(x)` vs. comparisons, function types vs.
//! parenthesized types) are resolved by speculative parsing: remember the
//! position, try one reading, and rewind if it fails.

use std::rc::Rc;

use crate::ast::*;
use crate::error::{CompileError, Span};
use crate::lexer::{Tok, Token};

type PResult<T> = Result<T, CompileError>;

pub fn parse_program(tokens: Vec<Token>) -> PResult<Vec<Stmt>> {
    let mut p = Parser::new(tokens);
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    Ok(out)
}

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Inside a `for (...` head: `in` is not a binary operator.
    no_in: bool,
    /// Counter for naming static blocks.
    static_blocks: u32,
    /// Parsing a conditional type's `extends` clause.
    no_conditional: bool,
}

const RESERVED: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
    "else", "enum", "export", "extends", "false", "finally", "for", "function", "if", "import",
    "in", "instanceof", "new", "null", "return", "super", "switch", "this", "throw", "true", "try",
    "typeof", "var", "void", "while", "with",
];

impl Parser {
    fn new(toks: Vec<Token>) -> Self {
        Parser {
            toks,
            pos: 0,
            no_in: false,
            static_blocks: 0,
            no_conditional: false,
        }
    }

    // ───────────── token helpers ─────────────

    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn tok_at(&self, off: usize) -> &Tok {
        let i = (self.pos + off).min(self.toks.len() - 1);
        &self.toks[i].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }
    fn nl_before(&self) -> bool {
        self.toks[self.pos].nl_before
    }
    fn at_eof(&self) -> bool {
        matches!(self.tok(), Tok::Eof)
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is(&self, p: &str) -> bool {
        matches!(self.tok(), Tok::P(q) if *q == p)
    }
    fn is_at(&self, off: usize, p: &str) -> bool {
        matches!(self.tok_at(off), Tok::P(q) if *q == p)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.tok(), Tok::Ident(n) if &**n == kw)
    }
    fn is_kw_at(&self, off: usize, kw: &str) -> bool {
        matches!(self.tok_at(off), Tok::Ident(n) if &**n == kw)
    }
    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        Err(CompileError::new(msg, self.span()))
    }
    fn describe(&self) -> String {
        match self.tok() {
            Tok::Num(n) => format!("number {n}"),
            Tok::BigInt(n) => format!("bigint {n}n"),
            Tok::Str(_) => "string".into(),
            Tok::Template { .. } => "template literal".into(),
            Tok::Regex { .. } => "regular expression".into(),
            Tok::Ident(n) => format!("'{n}'"),
            Tok::P(p) => format!("'{p}'"),
            Tok::Eof => "end of input".into(),
        }
    }
    fn expect(&mut self, p: &str) -> PResult<Span> {
        if self.is(p) {
            Ok(self.bump().span)
        } else {
            self.err(format!("expected '{p}', found {}", self.describe()))
        }
    }
    fn expect_kw(&mut self, kw: &str) -> PResult<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.err(format!("expected '{kw}', found {}", self.describe()))
        }
    }
    /// `>` in type-argument position; splits `>>`, `>>>`, `>=` and friends.
    fn expect_gt(&mut self) -> PResult<()> {
        let rest = match self.tok() {
            Tok::P(">") => {
                self.bump();
                return Ok(());
            }
            Tok::P(">>") => ">",
            Tok::P(">>>") => ">>",
            Tok::P(">=") => "=",
            Tok::P(">>=") => ">=",
            Tok::P(">>>=") => ">>=",
            _ => return self.err(format!("expected '>', found {}", self.describe())),
        };
        let t = &mut self.toks[self.pos];
        t.tok = Tok::P(rest);
        t.span.start += 1;
        t.nl_before = false;
        Ok(())
    }
    fn ident(&mut self) -> PResult<Name> {
        match self.tok().clone() {
            Tok::Ident(n) => {
                self.bump();
                Ok(n)
            }
            _ => self.err(format!("expected identifier, found {}", self.describe())),
        }
    }
    /// A binding identifier: not a reserved word.
    fn binding_ident(&mut self) -> PResult<Name> {
        if let Tok::Ident(n) = self.tok()
            && RESERVED.contains(&&**n)
        {
            return self.err(format!("'{n}' is a reserved word"));
        }
        self.ident()
    }
    /// Property name after `.` or in an object literal/type: any identifier,
    /// keyword, string or number.
    fn prop_name(&mut self) -> PResult<Name> {
        match self.tok().clone() {
            Tok::Ident(n) => {
                self.bump();
                Ok(n)
            }
            Tok::Str(s) => {
                self.bump();
                Ok(s)
            }
            Tok::Num(n) => {
                self.bump();
                Ok(crate::runtime::num_to_string(n).into())
            }
            Tok::P("#") => {
                self.bump();
                let n = self.ident()?;
                Ok(format!("#{n}").into())
            }
            _ => self.err(format!("expected property name, found {}", self.describe())),
        }
    }
    fn semi(&mut self) -> PResult<()> {
        if self.eat(";") || self.is("}") || self.at_eof() || self.nl_before() {
            Ok(())
        } else {
            self.err(format!("expected ';', found {}", self.describe()))
        }
    }

    /// Run `f`; if it fails, rewind and return None.
    fn try_parse<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> Option<T> {
        let save = self.pos;
        // expect_gt may split a token in place; snapshot the few tokens it could touch.
        let snapshot: Vec<(usize, Token)> = (save..(save + 64).min(self.toks.len()))
            .map(|i| (i, self.toks[i].clone()))
            .collect();
        match f(self) {
            Ok(v) => Some(v),
            Err(_) => {
                self.pos = save;
                for (i, t) in snapshot {
                    self.toks[i] = t;
                }
                None
            }
        }
    }

    // ───────────── statements ─────────────

    pub fn statement(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let kind = self.statement_kind()?;
        Ok(Stmt {
            kind,
            span: start.to(self.prev_span()),
        })
    }

    /// The string after `from` (or after `import` for a bare import).
    fn module_specifier(&mut self) -> PResult<Name> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.bump();
                Ok(s)
            }
            _ => self.err(format!("expected a module specifier string, found {}", self.describe())),
        }
    }

    /// Skip `with { type: "json" }` / `assert { … }` import attributes.
    fn import_attributes(&mut self) -> PResult<()> {
        if (self.is_kw("with") || self.is_kw("assert")) && !self.nl_before() && self.is_at(1, "{") {
            self.bump();
            self.bump();
            while !self.eat("}") {
                if self.at_eof() {
                    return self.err("expected '}'");
                }
                self.bump();
            }
        }
        Ok(())
    }

    fn import_decl(&mut self) -> PResult<StmtKind> {
        let span = self.span();
        self.expect_kw("import")?;
        let mut d = ImportDecl {
            default: None,
            namespace: None,
            named: vec![],
            from: "".into(),
            type_only: false,
            span,
        };
        if let Tok::Str(_) = self.tok() {
            d.from = self.module_specifier()?;
            self.import_attributes()?;
            self.semi()?;
            return Ok(StmtKind::Import(Rc::new(d)));
        }
        // `import type X from`, `import type { … }`, but not `import type from "m"`.
        if self.is_kw("type") && !self.is_kw_at(1, "from") && !self.is_at(1, ",") {
            self.bump();
            d.type_only = true;
        }
        if let Tok::Ident(_) = self.tok() {
            d.default = Some(self.binding_ident()?);
            if !self.eat(",") {
                return self.import_from(d);
            }
        }
        if self.eat("*") {
            self.expect_kw("as")?;
            d.namespace = Some(self.binding_ident()?);
        } else {
            self.expect("{")?;
            while !self.eat("}") {
                let mut type_only = false;
                if self.is_kw("type") && !self.is_at(1, ",") && !self.is_at(1, "}") && !self.is_kw_at(1, "as") {
                    self.bump();
                    type_only = true;
                }
                let imported = self.prop_name()?;
                let local = if self.eat_kw("as") { self.binding_ident()? } else { imported.clone() };
                d.named.push(ImportSpec { imported, local, type_only });
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
        }
        self.import_from(d)
    }

    fn import_from(&mut self, mut d: ImportDecl) -> PResult<StmtKind> {
        self.expect_kw("from")?;
        d.from = self.module_specifier()?;
        self.import_attributes()?;
        self.semi()?;
        Ok(StmtKind::Import(Rc::new(d)))
    }

    fn export_decl(&mut self) -> PResult<StmtKind> {
        let span = self.prev_span();
        if self.eat_kw("default") {
            let start = self.span();
            let is_decl = self.is_kw("function")
                || self.is_kw("class")
                || (self.is_kw("abstract") && self.is_kw_at(1, "class"))
                || (self.is_kw("async") && self.is_kw_at(1, "function"))
                || (self.is_kw("interface") && matches!(self.tok_at(1), Tok::Ident(_)));
            if is_decl {
                let kind = match &*self.ident_text() {
                    "function" => StmtKind::Func(Rc::new(self.function(true, false)?)),
                    "async" => {
                        self.bump();
                        StmtKind::Func(Rc::new(self.function(true, true)?))
                    }
                    "abstract" => {
                        self.bump();
                        StmtKind::Class(Rc::new(self.class(true)?))
                    }
                    "interface" => self.interface()?,
                    _ => StmtKind::Class(Rc::new(self.class(false)?)),
                };
                let stmt = Stmt {
                    kind,
                    span: start.to(self.prev_span()),
                };
                return Ok(StmtKind::Export(Rc::new(ExportDecl::DefaultDecl(stmt))));
            }
            let e = self.assign_expr()?;
            self.semi()?;
            return Ok(StmtKind::Export(Rc::new(ExportDecl::DefaultExpr(e))));
        }
        if self.eat("*") {
            let alias = if self.eat_kw("as") { Some(self.prop_name()?) } else { None };
            self.expect_kw("from")?;
            let from = self.module_specifier()?;
            self.import_attributes()?;
            self.semi()?;
            return Ok(StmtKind::Export(Rc::new(ExportDecl::All { from, alias, span })));
        }
        let type_only = self.is_kw("type") && self.is_at(1, "{");
        if type_only {
            self.bump();
        }
        if self.eat("{") {
            let mut specs = Vec::new();
            while !self.eat("}") {
                if self.is_kw("type") && !self.is_at(1, ",") && !self.is_at(1, "}") && !self.is_kw_at(1, "as") {
                    self.bump();
                }
                let local = self.prop_name()?;
                let exported = if self.eat_kw("as") { self.prop_name()? } else { local.clone() };
                specs.push((local, exported));
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            let from = if self.eat_kw("from") { Some(self.module_specifier()?) } else { None };
            self.import_attributes()?;
            self.semi()?;
            return Ok(StmtKind::Export(Rc::new(ExportDecl::Named { specs, from, span })));
        }
        let stmt = self.statement()?;
        Ok(StmtKind::Export(Rc::new(ExportDecl::Decl(stmt))))
    }

    /// `[Symbol.iterator]` as a member name: the symbol's property key.
    fn well_known_symbol_key(&mut self) -> Option<Name> {
        if self.is("[") && self.is_kw_at(1, "Symbol") && self.is_at(2, ".") && self.is_at(4, "]")
            && let Tok::Ident(n) = self.tok_at(3).clone()
        {
            for _ in 0..5 {
                self.bump();
            }
            return Some(format!("\u{2}Symbol.{n}").into());
        }
        None
    }

    fn ident_text(&self) -> Name {
        match self.tok() {
            Tok::Ident(n) => n.clone(),
            _ => "".into(),
        }
    }

    fn block_body(&mut self) -> PResult<Vec<Stmt>> {
        self.expect("{")?;
        let mut out = Vec::new();
        while !self.is("}") {
            if self.at_eof() {
                return self.err("expected '}'");
            }
            out.push(self.statement()?);
        }
        self.bump();
        Ok(out)
    }

    fn statement_kind(&mut self) -> PResult<StmtKind> {
        if self.is("{") {
            return Ok(StmtKind::Block(self.block_body()?));
        }
        if self.eat(";") {
            return Ok(StmtKind::Empty);
        }
        let Tok::Ident(kw) = self.tok().clone() else {
            return self.expr_statement();
        };
        // Label?
        if self.is_at(1, ":") && !RESERVED.contains(&&*kw) {
            self.bump();
            self.bump();
            let body = self.statement()?;
            return Ok(StmtKind::Labeled(kw, Box::new(body)));
        }
        match &*kw {
            "var" | "const" | "let" => {
                // `let` as an identifier (`let = 1`) is not supported.
                if &*kw == "const" && self.is_kw_at(1, "enum") {
                    self.bump();
                    return self.enum_decl();
                }
                let s = self.var_decl(false)?;
                self.semi()?;
                Ok(s)
            }
            "function" => Ok(StmtKind::Func(Rc::new(self.function(false, false)?))),
            "async" if self.is_kw_at(1, "function") && !self.toks[self.pos + 1].nl_before => {
                self.bump();
                Ok(StmtKind::Func(Rc::new(self.function(false, true)?)))
            }
            "class" => Ok(StmtKind::Class(Rc::new(self.class(false)?))),
            "abstract" if self.is_kw_at(1, "class") => {
                self.bump();
                Ok(StmtKind::Class(Rc::new(self.class(true)?)))
            }
            "interface" if matches!(self.tok_at(1), Tok::Ident(_)) => self.interface(),
            "type" if matches!(self.tok_at(1), Tok::Ident(_)) && !self.toks[self.pos + 1].nl_before => {
                self.bump();
                let name = self.ident()?;
                let tparams = self.type_params()?;
                self.expect("=")?;
                let ty = self.type_expr()?;
                self.semi()?;
                Ok(StmtKind::TypeAlias { name, tparams, ty })
            }
            "enum" if matches!(self.tok_at(1), Tok::Ident(_)) => self.enum_decl(),
            "declare" if matches!(self.tok_at(1), Tok::Ident(_)) && !self.toks[self.pos + 1].nl_before => {
                self.bump();
                if self.is_kw("var") || self.is_kw("let") || self.is_kw("const") {
                    let s = self.var_decl(true)?;
                    self.semi()?;
                    return Ok(s);
                }
                if self.is_kw("global") || self.is_kw("module") || self.is_kw("namespace") {
                    // Ambient declarations describe types only: skip them.
                    while !self.is("{") && !self.at_eof() {
                        self.bump();
                    }
                    self.block_body()?;
                    return Ok(StmtKind::Empty);
                }
                // declare function/class: parse and treat as a declaration with no body.
                self.statement_kind()
            }
            "export" if !self.is_at(1, "=") => {
                self.bump();
                self.export_decl()
            }
            "import" if !self.is_at(1, "(") && !self.is_at(1, ".") => self.import_decl(),
            "if" => {
                self.bump();
                self.expect("(")?;
                let test = self.expr()?;
                self.expect(")")?;
                let then = self.statement()?;
                let els = if self.eat_kw("else") {
                    Some(Box::new(self.statement()?))
                } else {
                    None
                };
                Ok(StmtKind::If(test, Box::new(then), els))
            }
            "while" => {
                self.bump();
                self.expect("(")?;
                let test = self.expr()?;
                self.expect(")")?;
                Ok(StmtKind::While(test, Box::new(self.statement()?)))
            }
            "do" => {
                self.bump();
                let body = self.statement()?;
                self.expect_kw("while")?;
                self.expect("(")?;
                let test = self.expr()?;
                self.expect(")")?;
                self.eat(";");
                Ok(StmtKind::DoWhile(Box::new(body), test))
            }
            "for" => self.for_stmt(),
            "return" => {
                self.bump();
                let value = if self.is(";") || self.is("}") || self.at_eof() || self.nl_before() {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.semi()?;
                Ok(StmtKind::Return(value))
            }
            "break" | "continue" => {
                self.bump();
                let label = match self.tok().clone() {
                    Tok::Ident(n) if !self.nl_before() && !RESERVED.contains(&&*n) => {
                        self.bump();
                        Some(n)
                    }
                    _ => None,
                };
                self.semi()?;
                Ok(if &*kw == "break" {
                    StmtKind::Break(label)
                } else {
                    StmtKind::Continue(label)
                })
            }
            "throw" => {
                self.bump();
                if self.nl_before() {
                    return self.err("illegal newline after throw");
                }
                let e = self.expr()?;
                self.semi()?;
                Ok(StmtKind::Throw(e))
            }
            "try" => {
                self.bump();
                let block = self.block_body()?;
                let mut param = None;
                let mut handler = None;
                if self.eat_kw("catch") {
                    if self.eat("(") {
                        param = Some(self.pattern()?);
                        if self.eat(":") {
                            self.type_expr()?;
                        }
                        self.expect(")")?;
                    }
                    handler = Some(self.block_body()?);
                }
                let finalizer = if self.eat_kw("finally") {
                    Some(self.block_body()?)
                } else {
                    None
                };
                if handler.is_none() && finalizer.is_none() {
                    return self.err("expected 'catch' or 'finally'");
                }
                Ok(StmtKind::Try {
                    block,
                    param,
                    handler,
                    finalizer,
                })
            }
            "switch" => {
                self.bump();
                self.expect("(")?;
                let disc = self.expr()?;
                self.expect(")")?;
                self.expect("{")?;
                let mut cases = Vec::new();
                while !self.eat("}") {
                    let test = if self.eat_kw("default") {
                        None
                    } else {
                        self.expect_kw("case")?;
                        Some(self.expr()?)
                    };
                    self.expect(":")?;
                    let mut body = Vec::new();
                    while !self.is_kw("case") && !self.is_kw("default") && !self.is("}") {
                        if self.at_eof() {
                            return self.err("expected '}'");
                        }
                        body.push(self.statement()?);
                    }
                    cases.push(Case { test, body });
                }
                Ok(StmtKind::Switch(disc, cases))
            }
            "debugger" => {
                self.bump();
                self.semi()?;
                Ok(StmtKind::Empty)
            }
            "with" => self.err("'with' is not supported"),
            "namespace" | "module" if matches!(self.tok_at(1), Tok::Ident(_)) && self.is_at(2, "{") => {
                self.bump();
                let name = self.ident()?;
                let body = self.block_body()?;
                Ok(StmtKind::Namespace { name, body })
            }
            _ => self.expr_statement(),
        }
    }

    fn expr_statement(&mut self) -> PResult<StmtKind> {
        let e = self.expr()?;
        self.semi()?;
        Ok(StmtKind::Expr(e))
    }

    fn enum_decl(&mut self) -> PResult<StmtKind> {
        self.expect_kw("enum")?;
        let name = self.ident()?;
        self.expect("{")?;
        let mut members = Vec::new();
        while !self.eat("}") {
            let sp = self.span();
            let m = self.prop_name()?;
            let init = if self.eat("=") { Some(self.assign_expr()?) } else { None };
            members.push((m, init, sp));
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(StmtKind::Enum { name, members })
    }

    fn var_decl(&mut self, declare: bool) -> PResult<StmtKind> {
        let kind = match &*self.ident()? {
            "var" => VarKind::Var,
            "let" => VarKind::Let,
            _ => VarKind::Const,
        };
        let mut decls = Vec::new();
        loop {
            let pat = self.pattern()?;
            self.eat("!"); // definite assignment assertion
            let ty = if self.eat(":") { Some(self.type_expr()?) } else { None };
            let init = if self.eat("=") { Some(self.assign_expr()?) } else { None };
            decls.push(VarDeclarator { pat, ty, init });
            if !self.eat(",") {
                break;
            }
        }
        Ok(StmtKind::Var { kind, decls, declare })
    }

    fn for_stmt(&mut self) -> PResult<StmtKind> {
        self.bump();
        let is_await = self.eat_kw("await");
        self.expect("(")?;
        let mut init = None;
        if !self.is(";") {
            let start = self.span();
            if self.is_kw("var") || self.is_kw("let") || self.is_kw("const") {
                let kind = match &*self.ident()? {
                    "var" => VarKind::Var,
                    "let" => VarKind::Let,
                    _ => VarKind::Const,
                };
                let pat = self.pattern()?;
                if self.eat_kw("of") {
                    let iter = self.assign_expr()?;
                    self.expect(")")?;
                    let body = self.statement()?;
                    return Ok(StmtKind::ForOf {
                        kind,
                        pat,
                        iter,
                        body: Box::new(body),
                        is_await,
                    });
                }
                if self.eat_kw("in") {
                    let Pattern::Ident(name, _) = pat else {
                        return self.err("destructuring in for-in is not supported");
                    };
                    let obj = self.expr()?;
                    self.expect(")")?;
                    let body = self.statement()?;
                    return Ok(StmtKind::ForIn {
                        kind,
                        name,
                        obj,
                        body: Box::new(body),
                    });
                }
                // Ordinary declaration list; re-parse from the first declarator.
                let ty = if self.eat(":") { Some(self.type_expr()?) } else { None };
                self.no_in = true;
                let first_init = if self.eat("=") { Some(self.assign_expr()?) } else { None };
                let mut decls = vec![VarDeclarator {
                    pat,
                    ty,
                    init: first_init,
                }];
                while self.eat(",") {
                    let pat = self.pattern()?;
                    let ty = if self.eat(":") { Some(self.type_expr()?) } else { None };
                    let init = if self.eat("=") { Some(self.assign_expr()?) } else { None };
                    decls.push(VarDeclarator { pat, ty, init });
                }
                self.no_in = false;
                init = Some(Box::new(Stmt {
                    kind: StmtKind::Var {
                        kind,
                        decls,
                        declare: false,
                    },
                    span: start.to(self.prev_span()),
                }));
            } else {
                self.no_in = true;
                let e = self.expr();
                self.no_in = false;
                let e = e?;
                if self.is_kw("of") || self.is_kw("in") {
                    return self.err("for-of/for-in over an existing variable is not supported; declare it with let/const");
                }
                init = Some(Box::new(Stmt {
                    span: e.span,
                    kind: StmtKind::Expr(e),
                }));
            }
        }
        self.expect(";")?;
        let test = if self.is(";") { None } else { Some(self.expr()?) };
        self.expect(";")?;
        let update = if self.is(")") { None } else { Some(self.expr()?) };
        self.expect(")")?;
        let body = self.statement()?;
        Ok(StmtKind::For {
            init,
            test,
            update,
            body: Box::new(body),
        })
    }

    fn interface(&mut self) -> PResult<StmtKind> {
        self.bump();
        let name = self.ident()?;
        let tparams = self.type_params()?;
        let mut extends = Vec::new();
        if self.eat_kw("extends") {
            loop {
                extends.push(self.type_primary()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        let members = self.type_members()?;
        Ok(StmtKind::Interface {
            name,
            tparams,
            extends,
            members,
        })
    }

    // ───────────── patterns ─────────────

    fn pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        if self.eat("{") {
            let mut props = Vec::new();
            let mut rest = None;
            while !self.eat("}") {
                if self.eat("...") {
                    rest = Some(Box::new(self.pattern()?));
                    self.eat(",");
                    self.expect("}")?;
                    break;
                }
                let ksp = self.span();
                let key = self.prop_name()?;
                let target = if self.eat(":") {
                    self.pattern()?
                } else {
                    Pattern::Ident(key.clone(), ksp)
                };
                let default = if self.eat("=") { Some(self.assign_expr()?) } else { None };
                props.push((key, target, default));
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            return Ok(Pattern::Object {
                props,
                rest,
                span: start.to(self.prev_span()),
            });
        }
        if self.eat("[") {
            let mut elems = Vec::new();
            let mut rest = None;
            while !self.eat("]") {
                if self.eat(",") {
                    elems.push(None);
                    continue;
                }
                if self.eat("...") {
                    rest = Some(Box::new(self.pattern()?));
                    self.expect("]")?;
                    break;
                }
                let p = self.pattern()?;
                let default = if self.eat("=") { Some(self.assign_expr()?) } else { None };
                elems.push(Some((p, default)));
                if !self.eat(",") {
                    self.expect("]")?;
                    break;
                }
            }
            return Ok(Pattern::Array {
                elems,
                rest,
                span: start.to(self.prev_span()),
            });
        }
        let n = self.binding_ident()?;
        Ok(Pattern::Ident(n, start))
    }

    // ───────────── functions & classes ─────────────

    fn params(&mut self) -> PResult<Vec<Param>> {
        self.expect("(")?;
        let mut out = Vec::new();
        while !self.eat(")") {
            let start = self.span();
            let mut property = false;
            while matches!(self.tok(), Tok::Ident(n) if matches!(&**n, "public"|"private"|"protected"|"readonly"|"override"))
                && matches!(self.tok_at(1), Tok::Ident(_) | Tok::P("{") | Tok::P("["))
            {
                self.bump();
                property = true;
            }
            let rest = self.eat("...");
            let pat = self.pattern()?;
            let optional = self.eat("?");
            let ty = if self.eat(":") { Some(self.type_expr()?) } else { None };
            let default = if self.eat("=") { Some(self.assign_expr()?) } else { None };
            // A `this` parameter only declares the type of `this`.
            if !matches!(&pat, Pattern::Ident(n, _) if &**n == "this") {
                out.push(Param {
                    pat,
                    ty,
                    optional,
                    default,
                    rest,
                    property,
                    span: start.to(self.prev_span()),
                });
            }
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(out)
    }

    fn type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut out = Vec::new();
        if !self.eat("<") {
            return Ok(out);
        }
        loop {
            self.eat_kw("const");
            let name = self.ident()?;
            let constraint = if self.eat_kw("extends") { Some(self.type_expr()?) } else { None };
            let default = if self.eat("=") { Some(self.type_expr()?) } else { None };
            out.push(TypeParam {
                name,
                constraint,
                default,
            });
            if !self.eat(",") {
                break;
            }
            if self.is(">") {
                break;
            }
        }
        self.expect_gt()?;
        Ok(out)
    }

    /// After the name: `<T>(params): Ret { body }` (body optional for overloads/declare).
    fn function_rest(&mut self, name: Option<Name>, start: Span, is_async: bool, is_generator: bool) -> PResult<Function> {
        let tparams = self.type_params()?;
        let params = self.params()?;
        let ret = if self.eat(":") { Some(self.return_type()?) } else { None };
        let body = if self.is("{") {
            Some(Body::Block(self.block_body()?))
        } else {
            self.semi()?;
            None
        };
        Ok(Function {
            name,
            tparams,
            params,
            ret,
            body,
            is_arrow: false,
            is_async,
            is_generator,
            span: start.to(self.prev_span()),
        })
    }

    /// Return type; also accepts type predicates `x is T` / `asserts x` (treated as boolean/void).
    fn return_type(&mut self) -> PResult<TypeExpr> {
        if self.is_kw("asserts") && matches!(self.tok_at(1), Tok::Ident(_)) {
            self.bump();
            self.bump();
            if self.eat_kw("is") {
                self.type_expr()?;
            }
            return Ok(TypeExpr::Ref {
                name: "void".into(),
                args: vec![],
                span: self.prev_span(),
            });
        }
        if matches!(self.tok(), Tok::Ident(_)) && self.is_kw_at(1, "is") && !self.toks[self.pos + 1].nl_before {
            let sp = self.span();
            self.bump();
            self.bump();
            self.type_expr()?;
            return Ok(TypeExpr::Ref {
                name: "boolean".into(),
                args: vec![],
                span: sp,
            });
        }
        self.type_expr()
    }

    fn function(&mut self, is_expr: bool, is_async: bool) -> PResult<Function> {
        let start = self.span();
        self.expect_kw("function")?;
        let is_generator = self.eat("*");
        let name = if matches!(self.tok(), Tok::Ident(_)) {
            Some(self.binding_ident()?)
        } else if is_expr {
            None
        } else {
            return self.err("expected function name");
        };
        self.function_rest(name, start, is_async, is_generator)
    }

    fn class(&mut self, is_abstract: bool) -> PResult<ClassDecl> {
        let start = self.span();
        self.expect_kw("class")?;
        let name = match self.tok() {
            Tok::Ident(n) if &**n != "extends" && &**n != "implements" => Some(self.binding_ident()?),
            _ => None,
        };
        let tparams = self.type_params()?;
        let extends = if self.eat_kw("extends") {
            let e = self.lhs_expr_no_call()?;
            let targs = if self.is("<") { self.type_args()? } else { vec![] };
            Some((e, targs))
        } else {
            None
        };
        let mut implements = Vec::new();
        if self.eat_kw("implements") {
            loop {
                implements.push(self.type_primary()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.expect("{")?;
        let mut members = Vec::new();
        while !self.eat("}") {
            if self.eat(";") {
                continue;
            }
            members.push(self.class_member()?);
        }
        Ok(ClassDecl {
            name,
            tparams,
            extends,
            implements,
            members,
            is_abstract,
            span: start.to(self.prev_span()),
        })
    }

    /// Whether the current identifier is used as a modifier (followed by a member name).
    fn modifier_follows(&self) -> bool {
        !self.toks[self.pos + 1].nl_before
            && matches!(self.tok_at(1), Tok::Ident(_) | Tok::Str(_) | Tok::Num(_) | Tok::P("[") | Tok::P("#") | Tok::P("*") | Tok::P("{"))
    }

    fn class_member(&mut self) -> PResult<ClassMember> {
        let start = self.span();
        let mut is_static = false;
        let mut is_async = false;
        let mut kind = MethodKind::Method;
        let mut is_abstract = false;
        loop {
            let Tok::Ident(n) = self.tok().clone() else { break };
            if !self.modifier_follows() {
                break;
            }
            match &*n {
                "public" | "private" | "protected" | "readonly" | "override" | "declare" | "accessor" => {}
                "abstract" => is_abstract = true,
                "static" => {
                    if self.is_at(1, "{") {
                        self.bump();
                        let span = self.span();
                        let body = self.block_body()?;
                        // A static block runs like a static method called once, when the class is defined.
                        self.static_blocks += 1;
                        let name: Name = format!("\u{1}static{}", self.static_blocks).into();
                        let func = Function {
                            name: Some(name.clone()),
                            tparams: vec![],
                            params: vec![],
                            ret: None,
                            body: Some(Body::Block(body)),
                            is_arrow: false,
                            is_async: false,
                            is_generator: false,
                            span,
                        };
                        return Ok(ClassMember::Method {
                            name,
                            func: Rc::new(func),
                            is_static: true,
                            kind: MethodKind::Method,
                        });
                    }
                    is_static = true
                }
                "async" => is_async = true,
                "get" => kind = MethodKind::Get,
                "set" => kind = MethodKind::Set,
                _ => break,
            }
            self.bump();
        }
        let is_generator = self.eat("*");
        let name = match self.well_known_symbol_key() {
            Some(k) => k,
            None if self.is("[") => return self.err("computed class member names are not supported (except well-known symbols)"),
            None => self.prop_name()?,
        };
        if &*name == "constructor" && self.is("(") {
            let f = self.function_rest(Some(name), start, false, false)?;
            return Ok(ClassMember::Constructor(Rc::new(f)));
        }
        if self.is("(") || self.is("<") {
            let mut f = self.function_rest(Some(name.clone()), start, is_async, is_generator)?;
            if is_abstract {
                f.body = None;
            }
            return Ok(ClassMember::Method {
                name,
                func: Rc::new(f),
                is_static,
                kind,
            });
        }
        let optional = self.eat("?");
        self.eat("!");
        let ty = if self.eat(":") { Some(self.type_expr()?) } else { None };
        let init = if self.eat("=") { Some(self.assign_expr()?) } else { None };
        self.semi()?;
        Ok(ClassMember::Field {
            name,
            ty,
            init,
            is_static,
            optional,
            span: start.to(self.prev_span()),
        })
    }

    // ───────────── types ─────────────

    pub fn type_expr(&mut self) -> PResult<TypeExpr> {
        // Function type: `<T>(…) => R`, `(…) => R`, `new (…) => R`.
        if self.is("<") {
            let tparams = self.type_params()?;
            let params = self.params()?;
            self.expect("=>")?;
            let ret = self.return_type()?;
            return Ok(TypeExpr::Func {
                tparams,
                params,
                ret: Box::new(ret),
            });
        }
        if self.is_kw("new") && self.is_at(1, "(") {
            self.bump();
            return self.type_expr();
        }
        if self.is("(")
            && let Some(t) = self.try_parse(|p| {
                let params = p.params()?;
                p.expect("=>")?;
                let ret = p.return_type()?;
                Ok(TypeExpr::Func {
                    tparams: vec![],
                    params,
                    ret: Box::new(ret),
                })
            })
        {
            return Ok(t);
        }
        let t = self.union_type()?;
        if self.is_kw("extends") && !self.nl_before() && !self.no_conditional {
            self.bump();
            // In the `extends` clause, a nested `extends` is a constraint on `infer`.
            let saved = self.no_conditional;
            self.no_conditional = true;
            let extends = self.type_expr();
            self.no_conditional = saved;
            let extends = extends?;
            self.expect("?")?;
            let then = self.type_expr()?;
            self.expect(":")?;
            let otherwise = self.type_expr()?;
            return Ok(TypeExpr::Conditional {
                check: Box::new(t),
                extends: Box::new(extends),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            });
        }
        Ok(t)
    }

    fn union_type(&mut self) -> PResult<TypeExpr> {
        self.eat("|");
        let mut parts = vec![self.intersection_type()?];
        while self.eat("|") {
            parts.push(self.intersection_type()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            TypeExpr::Union(parts)
        })
    }

    fn intersection_type(&mut self) -> PResult<TypeExpr> {
        self.eat("&");
        let mut parts = vec![self.postfix_type()?];
        while self.eat("&") {
            parts.push(self.postfix_type()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            TypeExpr::Intersection(parts)
        })
    }

    fn postfix_type(&mut self) -> PResult<TypeExpr> {
        if self.is_kw("keyof") {
            self.bump();
            return Ok(TypeExpr::KeyOf(Box::new(self.postfix_type()?)));
        }
        if self.is_kw("readonly") && !matches!(self.tok_at(1), Tok::P(_)) || (self.is_kw("readonly") && self.is_at(1, "[")) {
            self.bump();
        }
        let mut t = self.type_primary()?;
        while !self.nl_before() && self.is("[") {
            if self.is_at(1, "]") {
                self.bump();
                self.bump();
                t = TypeExpr::Array(Box::new(t));
            } else {
                self.bump();
                let saved = self.no_conditional;
                self.no_conditional = false;
                let k = self.type_expr();
                self.no_conditional = saved;
                let k = k?;
                self.expect("]")?;
                t = TypeExpr::Indexed(Box::new(t), Box::new(k));
            }
        }
        Ok(t)
    }

    /// `{ [K in …` or `{ readonly [K in …` / `{ -readonly [K in …`
    fn is_mapped_type(&self) -> bool {
        let mut i = 1;
        if matches!(self.tok_at(i), Tok::P("+") | Tok::P("-")) {
            i += 1;
        }
        if self.is_kw_at(i, "readonly") {
            i += 1;
        }
        self.is_at(i, "[") && matches!(self.tok_at(i + 1), Tok::Ident(_)) && self.is_kw_at(i + 2, "in")
    }

    fn mapped_type(&mut self) -> PResult<TypeExpr> {
        self.expect("{")?;
        let _ = self.eat("+") || self.eat("-");
        self.eat_kw("readonly");
        self.expect("[")?;
        let param = self.ident()?;
        self.expect_kw("in")?;
        let keys = self.type_expr()?;
        let rename = if self.eat_kw("as") { Some(Box::new(self.type_expr()?)) } else { None };
        self.expect("]")?;
        let optional = if self.eat("-") {
            self.expect("?")?;
            Some(false)
        } else {
            self.eat("+");
            if self.eat("?") { Some(true) } else { None }
        };
        self.expect(":")?;
        let value = self.type_expr()?;
        self.eat(";");
        self.eat(",");
        self.expect("}")?;
        Ok(TypeExpr::Mapped {
            param,
            keys: Box::new(keys),
            rename,
            value: Box::new(value),
            optional,
        })
    }

    fn type_args(&mut self) -> PResult<Vec<TypeExpr>> {
        self.expect("<")?;
        let mut out = Vec::new();
        if self.is(">") {
            self.bump();
            return Ok(out);
        }
        loop {
            out.push(self.type_expr()?);
            if !self.eat(",") {
                break;
            }
        }
        self.expect_gt()?;
        Ok(out)
    }

    fn type_primary(&mut self) -> PResult<TypeExpr> {
        let sp = self.span();
        match self.tok().clone() {
            Tok::P("(") => {
                self.bump();
                let t = self.type_expr()?;
                self.expect(")")?;
                Ok(t)
            }
            Tok::P("{") if self.is_mapped_type() => self.mapped_type(),
            Tok::P("{") => Ok(TypeExpr::Object(self.type_members()?)),
            Tok::Template { exprs, .. } if !exprs.is_empty() => {
                self.bump();
                Ok(TypeExpr::TemplateLit)
            }
            Tok::Ident(n) if &*n == "infer" && matches!(self.tok_at(1), Tok::Ident(_)) => {
                self.bump();
                let name = self.ident()?;
                if self.is_kw("extends") && self.no_conditional {
                    self.bump();
                    self.postfix_type()?;
                }
                Ok(TypeExpr::Infer(name))
            }
            Tok::P("[") => {
                self.bump();
                let mut elems = Vec::new();
                while !self.eat("]") {
                    // Named tuple members `[x: number]`.
                    if matches!(self.tok(), Tok::Ident(_)) && (self.is_at(1, ":") || (self.is_at(1, "?") && self.is_at(2, ":"))) {
                        self.bump();
                        self.eat("?");
                        self.bump();
                    }
                    self.eat("...");
                    let mut t = self.type_expr()?;
                    if self.eat("?") {
                        t = TypeExpr::Union(vec![
                            t,
                            TypeExpr::Ref {
                                name: "undefined".into(),
                                args: vec![],
                                span: sp,
                            },
                        ]);
                    }
                    elems.push(t);
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                Ok(TypeExpr::Tuple(elems))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(TypeExpr::StrLit(s))
            }
            Tok::Template { quasis, exprs, .. } if exprs.is_empty() => {
                self.bump();
                Ok(TypeExpr::StrLit(quasis[0].clone()))
            }
            Tok::Num(n) => {
                self.bump();
                Ok(TypeExpr::NumLit(n))
            }
            Tok::P("-") => {
                self.bump();
                match self.tok().clone() {
                    Tok::Num(n) => {
                        self.bump();
                        Ok(TypeExpr::NumLit(-n))
                    }
                    _ => self.err("expected number"),
                }
            }
            Tok::Ident(n) => {
                self.bump();
                match &*n {
                    "true" => return Ok(TypeExpr::BoolLit(true)),
                    "false" => return Ok(TypeExpr::BoolLit(false)),
                    "typeof" => {
                        let sp = self.span();
                        let mut name = self.ident()?.to_string();
                        while self.eat(".") {
                            name.push('.');
                            name.push_str(&self.prop_name()?);
                        }
                        return Ok(TypeExpr::TypeOf(name.into(), sp));
                    }
                    "infer" => return self.err("'infer' is not supported"),
                    "unique" if self.is_kw("symbol") => {
                        self.bump();
                        return self.err("symbols are not supported");
                    }
                    _ => {}
                }
                let mut name = n.to_string();
                while self.is(".") && matches!(self.tok_at(1), Tok::Ident(_)) {
                    self.bump();
                    name.push('.');
                    name.push_str(&self.ident()?);
                }
                let args = if self.is("<") && !self.nl_before() { self.type_args()? } else { vec![] };
                Ok(TypeExpr::Ref {
                    name: name.into(),
                    args,
                    span: sp.to(self.prev_span()),
                })
            }
            _ => self.err(format!("expected type, found {}", self.describe())),
        }
    }

    fn type_members(&mut self) -> PResult<Vec<TypeMember>> {
        self.expect("{")?;
        let mut out = Vec::new();
        while !self.eat("}") {
            if self.eat(";") || self.eat(",") {
                continue;
            }
            let mut readonly = false;
            if self.is_kw("readonly") && !matches!(self.tok_at(1), Tok::P("?") | Tok::P(":") | Tok::P("(")) {
                self.bump();
                readonly = true;
            }
            // Index signature `[k: string]: T`, or mapped type (unsupported).
            if self.is("[") && matches!(self.tok_at(1), Tok::Ident(_)) && self.is_at(2, ":") {
                self.bump();
                self.bump();
                self.bump();
                let key = self.type_expr()?;
                self.expect("]")?;
                self.expect(":")?;
                let value = self.type_expr()?;
                out.push(TypeMember::Index { key, value });
                continue;
            }
            if let Some(name) = self.well_known_symbol_key() {
                let optional = self.eat("?");
                if self.is("(") || self.is("<") {
                    let tparams = self.type_params()?;
                    let params = self.params()?;
                    self.expect(":")?;
                    let ret = self.return_type()?;
                    out.push(TypeMember::Prop {
                        name,
                        optional,
                        readonly,
                        ty: TypeExpr::Func { tparams, params, ret: Box::new(ret) },
                    });
                } else {
                    self.expect(":")?;
                    let ty = self.type_expr()?;
                    out.push(TypeMember::Prop { name, optional, readonly, ty });
                }
                continue;
            }
            if self.is("[") {
                return self.err("mapped and computed-key types are not supported");
            }
            if self.is("(") || self.is("<") {
                let tparams = self.type_params()?;
                let params = self.params()?;
                self.expect(":")?;
                let ret = self.return_type()?;
                out.push(TypeMember::Call(TypeExpr::Func {
                    tparams,
                    params,
                    ret: Box::new(ret),
                }));
                continue;
            }
            if self.is_kw("new") && (self.is_at(1, "(") || self.is_at(1, "<")) {
                return self.err("construct signatures are not supported");
            }
            // get/set accessors in types: treat as properties.
            if (self.is_kw("get") || self.is_kw("set")) && matches!(self.tok_at(1), Tok::Ident(_)) {
                self.bump();
            }
            let name = self.prop_name()?;
            let optional = self.eat("?");
            if self.is("(") || self.is("<") {
                let tparams = self.type_params()?;
                let params = self.params()?;
                let ret = if self.eat(":") {
                    self.return_type()?
                } else {
                    TypeExpr::Ref {
                        name: "any".into(),
                        args: vec![],
                        span: self.prev_span(),
                    }
                };
                out.push(TypeMember::Prop {
                    name,
                    optional,
                    readonly,
                    ty: TypeExpr::Func {
                        tparams,
                        params,
                        ret: Box::new(ret),
                    },
                });
                continue;
            }
            let ty = if self.eat(":") {
                self.type_expr()?
            } else {
                TypeExpr::Ref {
                    name: "any".into(),
                    args: vec![],
                    span: self.prev_span(),
                }
            };
            out.push(TypeMember::Prop {
                name,
                optional,
                readonly,
                ty,
            });
        }
        Ok(out)
    }

    // ───────────── expressions ─────────────

    pub fn expr(&mut self) -> PResult<Expr> {
        let first = self.assign_expr()?;
        if !self.is(",") {
            return Ok(first);
        }
        let mut list = vec![first];
        while self.eat(",") {
            list.push(self.assign_expr()?);
        }
        let span = list[0].span.to(list.last().unwrap().span);
        Ok(Expr {
            kind: ExprKind::Seq(list),
            span,
        })
    }

    fn mk(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }

    /// Try to parse an arrow function at the current position.
    fn try_arrow(&mut self) -> PResult<Option<Expr>> {
        let start = self.span();
        if self.is_kw("async") && !self.toks[self.pos + 1].nl_before && matches!(self.tok_at(1), Tok::Ident(_) | Tok::P("(") | Tok::P("<")) {
            // `async x => …` or `async (…) => …`
            let save = self.pos;
            self.bump();
            if let Some(e) = self.try_arrow()? {
                let ExprKind::Func(f) = e.kind else { unreachable!() };
                let mut f = (*f).clone();
                f.is_async = true;
                return Ok(Some(Self::mk(ExprKind::Func(Rc::new(f)), start.to(e.span))));
            }
            self.pos = save;
            return Ok(None);
        }
        // `x => …`
        if let Tok::Ident(n) = self.tok().clone()
            && self.is_at(1, "=>")
            && !RESERVED.contains(&&*n)
        {
            let sp = self.span();
            self.bump();
            self.bump();
            let body = self.arrow_body()?;
            let f = Function {
                name: None,
                tparams: vec![],
                params: vec![Param {
                    pat: Pattern::Ident(n, sp),
                    ty: None,
                    optional: false,
                    default: None,
                    rest: false,
                    property: false,
                    span: sp,
                }],
                ret: None,
                body: Some(body),
                is_arrow: true,
                is_async: false,
                is_generator: false,
                span: start.to(self.prev_span()),
            };
            return Ok(Some(Self::mk(ExprKind::Func(Rc::new(f)), start.to(self.prev_span()))));
        }
        if !self.is("(") && !self.is("<") {
            return Ok(None);
        }
        let head = self.try_parse(|p| {
            let tparams = p.type_params()?;
            let params = p.params()?;
            let ret = if p.eat(":") { Some(p.return_type()?) } else { None };
            if p.nl_before() || !p.is("=>") {
                return p.err("not an arrow");
            }
            p.bump();
            Ok((tparams, params, ret))
        });
        let Some((tparams, params, ret)) = head else {
            return Ok(None);
        };
        let body = self.arrow_body()?;
        let f = Function {
            name: None,
            tparams,
            params,
            ret,
            body: Some(body),
            is_arrow: true,
            is_async: false,
            is_generator: false,
            span: start.to(self.prev_span()),
        };
        Ok(Some(Self::mk(ExprKind::Func(Rc::new(f)), start.to(self.prev_span()))))
    }

    fn arrow_body(&mut self) -> PResult<Body> {
        if self.is("{") {
            Ok(Body::Block(self.block_body()?))
        } else {
            let save = self.no_in;
            self.no_in = false;
            let e = self.assign_expr();
            self.no_in = save;
            Ok(Body::Expr(Box::new(e?)))
        }
    }

    pub fn assign_expr(&mut self) -> PResult<Expr> {
        if let Some(arrow) = self.try_arrow()? {
            return Ok(arrow);
        }
        if self.is_kw("yield") {
            let start = self.span();
            self.bump();
            let delegate = !self.nl_before() && self.eat("*");
            let ends = self.nl_before() && !delegate
                || matches!(self.tok(), Tok::P(")" | "]" | "}" | "," | ";" | ":") | Tok::Eof);
            let arg = if ends { None } else { Some(Box::new(self.assign_expr()?)) };
            let span = start.to(self.prev_span());
            return Ok(Self::mk(ExprKind::Yield { arg, delegate }, span));
        }
        let start = self.span();
        let lhs = self.cond_expr()?;
        let op = match self.tok() {
            Tok::P(p) => match *p {
                "=" => Some(AssignOp::Assign),
                "+=" => Some(AssignOp::Bin(BinOp::Add)),
                "-=" => Some(AssignOp::Bin(BinOp::Sub)),
                "*=" => Some(AssignOp::Bin(BinOp::Mul)),
                "/=" => Some(AssignOp::Bin(BinOp::Div)),
                "%=" => Some(AssignOp::Bin(BinOp::Mod)),
                "**=" => Some(AssignOp::Bin(BinOp::Exp)),
                "<<=" => Some(AssignOp::Bin(BinOp::Shl)),
                ">>=" => Some(AssignOp::Bin(BinOp::Shr)),
                ">>>=" => Some(AssignOp::Bin(BinOp::UShr)),
                "&=" => Some(AssignOp::Bin(BinOp::BitAnd)),
                "|=" => Some(AssignOp::Bin(BinOp::BitOr)),
                "^=" => Some(AssignOp::Bin(BinOp::BitXor)),
                "&&=" => Some(AssignOp::Logical(LogicalOp::And)),
                "||=" => Some(AssignOp::Logical(LogicalOp::Or)),
                "??=" => Some(AssignOp::Logical(LogicalOp::Nullish)),
                _ => None,
            },
            _ => None,
        };
        let Some(op) = op else { return Ok(lhs) };
        self.bump();
        let rhs = self.assign_expr()?;
        let span = start.to(rhs.span);
        Ok(Self::mk(ExprKind::Assign(op, Box::new(lhs), Box::new(rhs)), span))
    }

    fn cond_expr(&mut self) -> PResult<Expr> {
        let test = self.binary_expr(0)?;
        if !self.eat("?") {
            return Ok(test);
        }
        let save = self.no_in;
        self.no_in = false;
        let then = self.assign_expr();
        self.no_in = save;
        let then = then?;
        self.expect(":")?;
        let els = self.assign_expr()?;
        let span = test.span.to(els.span);
        Ok(Self::mk(ExprKind::Cond(Box::new(test), Box::new(then), Box::new(els)), span))
    }

    /// Binary operator at the current token: (precedence, kind).
    fn binop(&self) -> Option<(u8, Result<BinOp, LogicalOp>)> {
        use BinOp::*;
        let r = match self.tok() {
            Tok::P(p) => match *p {
                "??" => (1, Err(LogicalOp::Nullish)),
                "||" => (1, Err(LogicalOp::Or)),
                "&&" => (2, Err(LogicalOp::And)),
                "|" => (3, Ok(BitOr)),
                "^" => (4, Ok(BitXor)),
                "&" => (5, Ok(BitAnd)),
                "==" => (6, Ok(Eq)),
                "!=" => (6, Ok(Ne)),
                "===" => (6, Ok(StrictEq)),
                "!==" => (6, Ok(StrictNe)),
                "<" => (7, Ok(Lt)),
                ">" => (7, Ok(Gt)),
                "<=" => (7, Ok(Le)),
                ">=" => (7, Ok(Ge)),
                "<<" => (8, Ok(Shl)),
                ">>" => (8, Ok(Shr)),
                ">>>" => (8, Ok(UShr)),
                "+" => (9, Ok(Add)),
                "-" => (9, Ok(Sub)),
                "*" => (10, Ok(Mul)),
                "/" => (10, Ok(Div)),
                "%" => (10, Ok(Mod)),
                "**" => (11, Ok(Exp)),
                _ => return None,
            },
            Tok::Ident(n) => match &**n {
                "instanceof" => (7, Ok(InstanceOf)),
                "in" if !self.no_in => (7, Ok(In)),
                _ => return None,
            },
            _ => return None,
        };
        Some(r)
    }

    fn binary_expr(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary_expr()?;
        loop {
            // `x as T` / `x satisfies T` bind at relational precedence.
            if (self.is_kw("as") || self.is_kw("satisfies")) && !self.nl_before() && 7 > min_prec {
                let sat = self.is_kw("satisfies");
                self.bump();
                if self.is_kw("const") {
                    self.bump();
                    let span = lhs.span.to(self.prev_span());
                    lhs = Self::mk(ExprKind::Paren(Box::new(lhs)), span);
                    continue;
                }
                let ty = self.type_expr()?;
                let span = lhs.span.to(self.prev_span());
                lhs = Self::mk(
                    if sat {
                        ExprKind::Satisfies(Box::new(lhs), ty)
                    } else {
                        ExprKind::As(Box::new(lhs), ty)
                    },
                    span,
                );
                continue;
            }
            let Some((prec, op)) = self.binop() else { break };
            if prec <= min_prec {
                break;
            }
            self.bump();
            // `**` is right-associative.
            let rhs = if op == Ok(BinOp::Exp) {
                self.binary_expr(prec - 1)?
            } else {
                self.binary_expr(prec)?
            };
            let span = lhs.span.to(rhs.span);
            lhs = Self::mk(
                match op {
                    Ok(b) => ExprKind::Binary(b, Box::new(lhs), Box::new(rhs)),
                    Err(l) => ExprKind::Logical(l, Box::new(lhs), Box::new(rhs)),
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn unary_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.tok() {
            Tok::P("!") => Some(UnaryOp::Not),
            Tok::P("-") => Some(UnaryOp::Neg),
            Tok::P("+") => Some(UnaryOp::Plus),
            Tok::P("~") => Some(UnaryOp::BitNot),
            Tok::Ident(n) if &**n == "typeof" => Some(UnaryOp::TypeOf),
            Tok::Ident(n) if &**n == "void" => Some(UnaryOp::Void),
            Tok::Ident(n) if &**n == "delete" => Some(UnaryOp::Delete),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let e = self.unary_expr()?;
            let span = start.to(e.span);
            return Ok(Self::mk(ExprKind::Unary(op, Box::new(e)), span));
        }
        if self.is("++") || self.is("--") {
            let inc = self.is("++");
            self.bump();
            let e = self.unary_expr()?;
            let span = start.to(e.span);
            return Ok(Self::mk(
                ExprKind::Update {
                    inc,
                    prefix: true,
                    target: Box::new(e),
                },
                span,
            ));
        }
        if self.is_kw("await") && !matches!(self.tok_at(1), Tok::P(")") | Tok::P(";") | Tok::P(",") | Tok::P("=") | Tok::P(".")) {
            self.bump();
            let e = self.unary_expr()?;
            let span = start.to(e.span);
            return Ok(Self::mk(ExprKind::Await(Box::new(e)), span));
        }
        if self.is("<") {
            return self.err("angle-bracket type assertions are not supported; use 'as'");
        }
        let e = self.postfix_expr()?;
        if (self.is("++") || self.is("--")) && !self.nl_before() {
            let inc = self.is("++");
            self.bump();
            let span = e.span.to(self.prev_span());
            return Ok(Self::mk(
                ExprKind::Update {
                    inc,
                    prefix: false,
                    target: Box::new(e),
                },
                span,
            ));
        }
        Ok(e)
    }

    fn args(&mut self) -> PResult<Vec<ArrayElem>> {
        self.expect("(")?;
        let mut out = Vec::new();
        let save = self.no_in;
        self.no_in = false;
        while !self.eat(")") {
            if self.eat("...") {
                out.push(ArrayElem::Spread(self.assign_expr()?));
            } else {
                out.push(ArrayElem::Expr(self.assign_expr()?));
            }
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        self.no_in = save;
        Ok(out)
    }

    /// Member/call chain. Wraps the result in `OptChain` if any `?.` occurred.
    fn postfix_expr(&mut self) -> PResult<Expr> {
        let mut e = self.primary_expr()?;
        let mut has_opt = false;
        loop {
            let start = e.span;
            if self.eat(".") {
                let prop = self.prop_name()?;
                e = Self::mk(
                    ExprKind::Member {
                        obj: Box::new(e),
                        prop,
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else if self.eat("?.") {
                has_opt = true;
                if self.is("(") {
                    let args = self.args()?;
                    e = Self::mk(
                        ExprKind::Call {
                            callee: Box::new(e),
                            targs: vec![],
                            args,
                            optional: true,
                        },
                        start.to(self.prev_span()),
                    );
                } else if self.eat("[") {
                    let index = self.expr()?;
                    self.expect("]")?;
                    e = Self::mk(
                        ExprKind::Index {
                            obj: Box::new(e),
                            index: Box::new(index),
                            optional: true,
                        },
                        start.to(self.prev_span()),
                    );
                } else {
                    let prop = self.prop_name()?;
                    e = Self::mk(
                        ExprKind::Member {
                            obj: Box::new(e),
                            prop,
                            optional: true,
                        },
                        start.to(self.prev_span()),
                    );
                }
            } else if self.is("[") {
                self.bump();
                let save = self.no_in;
                self.no_in = false;
                let index = self.expr();
                self.no_in = save;
                let index = index?;
                self.expect("]")?;
                e = Self::mk(
                    ExprKind::Index {
                        obj: Box::new(e),
                        index: Box::new(index),
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else if self.is("(") {
                let args = self.args()?;
                e = Self::mk(
                    ExprKind::Call {
                        callee: Box::new(e),
                        targs: vec![],
                        args,
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else if self.is("<")
                && let Some(targs) = self.try_parse(|p| {
                    let t = p.type_args()?;
                    if !p.is("(") {
                        return p.err("not a generic call");
                    }
                    Ok(t)
                })
            {
                let args = self.args()?;
                e = Self::mk(
                    ExprKind::Call {
                        callee: Box::new(e),
                        targs,
                        args,
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else if self.is("!") && !self.nl_before() && !self.is_at(1, "=") {
                self.bump();
                e = Self::mk(ExprKind::NonNull(Box::new(e)), start.to(self.prev_span()));
            } else if let Tok::Template { quasis, exprs, raws } = self.tok().clone() {
                // tag`a${x}b` → tag(<strings array>, x)
                self.bump();
                let tspan = start.to(self.prev_span());
                let lit = |kind: ExprKind| ArrayElem::Expr(Self::mk(kind, tspan));
                let mut strings = vec![lit(ExprKind::Num(quasis.len() as f64))];
                strings.extend(quasis.iter().map(|q| lit(ExprKind::Str(q.clone()))));
                strings.extend(raws.iter().map(|q| lit(ExprKind::Str(q.clone()))));
                let template_object = Self::mk(
                    ExprKind::Call {
                        callee: Box::new(Self::mk(ExprKind::Ident("\u{1}templateObject".into()), tspan)),
                        targs: vec![],
                        args: strings,
                        optional: false,
                    },
                    tspan,
                );
                let mut args = vec![ArrayElem::Expr(template_object)];
                for toks in exprs {
                    let mut sub = Parser::new(toks);
                    let x = sub.expr()?;
                    args.push(ArrayElem::Expr(x));
                }
                e = Self::mk(
                    ExprKind::Call {
                        callee: Box::new(e),
                        targs: vec![],
                        args,
                        optional: false,
                    },
                    tspan,
                );
            } else {
                break;
            }
        }
        if has_opt {
            let span = e.span;
            e = Self::mk(ExprKind::OptChain(Box::new(e)), span);
        }
        Ok(e)
    }

    /// Callee of `new` / superclass: primary with member accesses but no calls.
    fn lhs_expr_no_call(&mut self) -> PResult<Expr> {
        let mut e = if self.is_kw("new") {
            self.new_expr()?
        } else {
            self.primary_expr()?
        };
        loop {
            let start = e.span;
            if self.eat(".") {
                let prop = self.prop_name()?;
                e = Self::mk(
                    ExprKind::Member {
                        obj: Box::new(e),
                        prop,
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else if self.eat("[") {
                let index = self.expr()?;
                self.expect("]")?;
                e = Self::mk(
                    ExprKind::Index {
                        obj: Box::new(e),
                        index: Box::new(index),
                        optional: false,
                    },
                    start.to(self.prev_span()),
                );
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn new_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect_kw("new")?;
        if self.is(".") {
            return self.err("new.target is not supported");
        }
        let callee = self.lhs_expr_no_call()?;
        let targs = if self.is("<") { self.type_args()? } else { vec![] };
        let args = if self.is("(") { self.args()? } else { vec![] };
        Ok(Self::mk(
            ExprKind::New {
                callee: Box::new(callee),
                targs,
                args,
            },
            start.to(self.prev_span()),
        ))
    }

    fn primary_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        let t = self.tok().clone();
        match t {
            Tok::Num(n) => {
                self.bump();
                Ok(Self::mk(ExprKind::Num(n), start))
            }
            Tok::BigInt(n) => {
                self.bump();
                Ok(Self::mk(ExprKind::BigInt(n), start))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Self::mk(ExprKind::Str(s), start))
            }
            Tok::Regex { pattern, flags } => {
                self.bump();
                Ok(Self::mk(ExprKind::Regex(pattern, flags), start))
            }
            Tok::Template { quasis, exprs, .. } => {
                self.bump();
                let mut es = Vec::new();
                for toks in exprs {
                    let mut sub = Parser::new(toks);
                    let e = sub.expr()?;
                    if !sub.at_eof() {
                        return sub.err("unexpected token in template expression");
                    }
                    es.push(e);
                }
                Ok(Self::mk(ExprKind::Template { quasis, exprs: es }, start))
            }
            Tok::P("(") => {
                self.bump();
                let save = self.no_in;
                self.no_in = false;
                let e = self.expr();
                self.no_in = save;
                let e = e?;
                self.expect(")")?;
                Ok(Self::mk(ExprKind::Paren(Box::new(e)), start.to(self.prev_span())))
            }
            Tok::P("[") => {
                self.bump();
                let mut elems = Vec::new();
                while !self.eat("]") {
                    if self.is(",") {
                        self.bump();
                        elems.push(ArrayElem::Hole);
                        continue;
                    }
                    if self.eat("...") {
                        elems.push(ArrayElem::Spread(self.assign_expr()?));
                    } else {
                        elems.push(ArrayElem::Expr(self.assign_expr()?));
                    }
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                Ok(Self::mk(ExprKind::Array(elems), start.to(self.prev_span())))
            }
            Tok::P("{") => self.object_literal(),
            Tok::Ident(n) => match &*n {
                "true" | "false" => {
                    self.bump();
                    Ok(Self::mk(ExprKind::Bool(&*n == "true"), start))
                }
                "null" => {
                    self.bump();
                    Ok(Self::mk(ExprKind::Null, start))
                }
                "this" => {
                    self.bump();
                    Ok(Self::mk(ExprKind::This, start))
                }
                "super" => {
                    self.bump();
                    Ok(Self::mk(ExprKind::Super, start))
                }
                "function" => {
                    let f = self.function(true, false)?;
                    Ok(Self::mk(ExprKind::Func(Rc::new(f)), start.to(self.prev_span())))
                }
                "async" if self.is_kw_at(1, "function") => {
                    self.bump();
                    let f = self.function(true, true)?;
                    Ok(Self::mk(ExprKind::Func(Rc::new(f)), start.to(self.prev_span())))
                }
                "class" => {
                    let c = self.class(false)?;
                    Ok(Self::mk(ExprKind::Class(Rc::new(c)), start.to(self.prev_span())))
                }
                "new" => self.new_expr(),
                // `import.meta` and `import("x")`: rewritten by the module loader.
                "import" if self.is_at(1, ".") || self.is_at(1, "(") => {
                    self.bump();
                    Ok(Self::mk(ExprKind::Ident(n), start))
                }
                _ if RESERVED.contains(&&*n) => self.err(format!("unexpected keyword '{n}'")),
                _ => {
                    self.bump();
                    Ok(Self::mk(ExprKind::Ident(n), start))
                }
            },
            Tok::P("/") | Tok::P("/=") => self.err("regular expressions are not supported yet"),
            _ => self.err(format!("unexpected {}", self.describe())),
        }
    }

    fn object_literal(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect("{")?;
        let save = self.no_in;
        self.no_in = false;
        let mut props = Vec::new();
        while !self.eat("}") {
            if self.eat("...") {
                props.push(ObjProp::Spread(self.assign_expr()?));
            } else {
                let mut is_async = false;
                let mut kind = MethodKind::Method;
                if let Tok::Ident(n) = self.tok().clone()
                    && matches!(&*n, "get" | "set" | "async")
                    && matches!(self.tok_at(1), Tok::Ident(_) | Tok::Str(_) | Tok::Num(_) | Tok::P("["))
                {
                    self.bump();
                    match &*n {
                        "get" => kind = MethodKind::Get,
                        "set" => kind = MethodKind::Set,
                        _ => is_async = true,
                    }
                }
                if kind != MethodKind::Method {
                    let ksp = self.span();
                    let key = match self.well_known_symbol_key() {
                        Some(k) => k,
                        None => self.prop_name()?,
                    };
                    let f = self.function_rest(Some(key.clone()), ksp, false, false)?;
                    props.push(ObjProp::Accessor {
                        key,
                        is_get: kind == MethodKind::Get,
                        func: Rc::new(f),
                    });
                    if !self.eat(",") {
                        self.expect("}")?;
                        break;
                    }
                    continue;
                }
                let is_generator = self.eat("*");
                let ksp = self.span();
                let key = if let Some(k) = self.well_known_symbol_key() {
                    PropKey::Name(k)
                } else if self.eat("[") {
                    let e = self.assign_expr()?;
                    self.expect("]")?;
                    PropKey::Computed(e)
                } else {
                    PropKey::Name(self.prop_name()?)
                };
                if self.is("(") || self.is("<") {
                    let name = match &key {
                        PropKey::Name(n) => Some(n.clone()),
                        _ => None,
                    };
                    let f = self.function_rest(name, ksp, is_async, is_generator)?;
                    let span = f.span;
                    props.push(ObjProp::Init {
                        key,
                        value: Self::mk(ExprKind::Func(Rc::new(f)), span),
                    });
                } else if self.eat(":") {
                    let value = self.assign_expr()?;
                    props.push(ObjProp::Init { key, value });
                } else {
                    let PropKey::Name(n) = &key else {
                        return self.err("expected ':'");
                    };
                    props.push(ObjProp::Init {
                        value: Self::mk(ExprKind::Ident(n.clone()), ksp),
                        key,
                    });
                }
            }
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        self.no_in = save;
        Ok(Self::mk(ExprKind::Object(props), start.to(self.prev_span())))
    }
}
