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
