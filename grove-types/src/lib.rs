use std::{borrow::Cow, ops::Range};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub fn merge(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

impl From<Range<usize>> for Span {
    fn from(value: Range<usize>) -> Self {
        Span::new(value.start, value.end)
    }
}

/// Unique identity of a typechecked node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub span: Span,
    pub value: T,
}

impl<T> Spanned<T> {
    pub fn new(span: Span, value: T) -> Spanned<T> {
        Spanned { span, value }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            span: self.span,
            value: f(self.value),
        }
    }

    pub fn as_ref(&self) -> Spanned<&T> {
        Spanned {
            span: self.span,
            value: &self.value,
        }
    }
}

impl<T> std::ops::Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

pub type DiagStr = Cow<'static, str>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelStyle {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub span: Span,
    pub message: DiagStr,
    pub style: LabelStyle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: DiagStr,
    pub message: DiagStr,
    pub labels: Vec<Label>,
    pub notes: Vec<DiagStr>,
    pub help: Vec<DiagStr>,
}

impl Diagnostic {
    pub fn error(
        code: impl Into<DiagStr>,
        message: impl Into<DiagStr>,
        span: Span,
        label: impl Into<DiagStr>,
    ) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            labels: vec![Label {
                span,
                message: label.into(),
                style: LabelStyle::Primary,
            }],
            notes: Vec::new(),
            help: Vec::new(),
        }
    }

    pub fn warning(
        code: impl Into<DiagStr>,
        message: impl Into<DiagStr>,
        span: Span,
        label: impl Into<DiagStr>,
    ) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code: code.into(),
            message: message.into(),
            labels: vec![Label {
                span,
                message: label.into(),
                style: LabelStyle::Primary,
            }],
            notes: Vec::new(),
            help: Vec::new(),
        }
    }

    pub fn with_secondary_label(self, span: Span, message: impl Into<DiagStr>) -> Self {
        self.with_label(Label {
            span,
            message: message.into(),
            style: LabelStyle::Secondary,
        })
    }

    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    pub fn with_note(mut self, note: impl Into<DiagStr>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<DiagStr>) -> Self {
        self.help.push(help.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_helpers() {
        let a = Span::new(2, 5);
        let b = Span::new(7, 9);
        assert_eq!(a.merge(b), Span::new(2, 9));
        assert_eq!(a.len(), 3);
        assert!(!a.is_empty());
        assert!(Span::new(2, 2).is_empty());
    }
}
