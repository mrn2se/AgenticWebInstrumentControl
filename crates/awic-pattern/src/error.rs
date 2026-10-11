//! Parse errors with a byte span, so a caller (and an agent that wrote the
//! pattern) can point at the offending token (SPEC §2.9.1).

use std::fmt;

/// A byte-offset span into the original pattern text, `start..end`
/// (end-exclusive). Empty spans (`start == end`) point just past the last
/// token, for "expected more input" errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn at(pos: usize) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }
}

/// A pattern failed to parse. `message` is written to be actionable on its
/// own — an agent should be able to fix its pattern from the message alone,
/// with no other context (SPEC §2.9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl ParseError {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (at byte {}..{})",
            self.message, self.span.start, self.span.end
        )
    }
}

impl std::error::Error for ParseError {}
