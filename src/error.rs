use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span {
            start: start as u32,
            end: end as u32,
        }
    }
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// A parse or type error, located in the source.
#[derive(Debug, Clone)]
pub struct CompileError {
    pub message: String,
    pub span: Span,
}

impl CompileError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        CompileError {
            message: message.into(),
            span,
        }
    }

    /// `file:line:col: error: message` plus the offending source line.
    pub fn render(&self, file: &str, src: &str) -> String {
        let off = (self.span.start as usize).min(src.len());
        let before = &src[..off];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let col = src[line_start..off].chars().count() + 1;
        let line_text = src[line_start..].lines().next().unwrap_or("");
        format!(
            "{file}:{line}:{col}: error: {}\n    {line_text}\n    {}^",
            self.message,
            " ".repeat(col - 1)
        )
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Every source file of a program, laid out one after another in a single
/// offset space: a span's offset identifies both the file and the position.
#[derive(Debug, Default)]
pub struct Sources {
    pub files: Vec<SourceFile>,
    next: usize,
}

#[derive(Debug)]
pub struct SourceFile {
    /// Path as shown in messages.
    pub name: String,
    pub src: String,
    pub base: usize,
}

impl Sources {
    /// Add a file; returns its base offset (pass it to `lexer::tokenize_at`).
    pub fn add(&mut self, name: String, src: String) -> usize {
        let base = self.next;
        // Leave a gap so an end-of-file span never lands in the next file.
        self.next += src.len() + 1;
        self.files.push(SourceFile { name, src, base });
        base
    }

    pub fn file_at(&self, offset: u32) -> Option<&SourceFile> {
        self.files.iter().rev().find(|f| f.base <= offset as usize)
    }

    /// `file:line:col: error: message` plus the offending source line.
    pub fn render(&self, e: &CompileError) -> String {
        match self.file_at(e.span.start) {
            Some(f) => {
                let local = CompileError {
                    message: e.message.clone(),
                    span: Span {
                        start: e.span.start - f.base as u32,
                        end: e.span.end.saturating_sub(f.base as u32),
                    },
                };
                local.render(&f.name, &f.src)
            }
            None => format!("error: {}", e.message),
        }
    }
}
