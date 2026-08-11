//! LSP diagnostic types (a small neutral subset). Ported from production `lsp/types.rs`.

use serde::{Deserialize, Serialize};

/// A source location normalized for model-facing output. Lines and columns are
/// one-based even though LSP uses zero-based positions on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

impl Location {
    pub fn display_line(&self) -> String {
        format!("{}:{}:{}", self.file, self.line, self.column)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

impl DiagnosticSeverity {
    pub fn from_lsp(value: u64) -> Self {
        match value {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Info,
            _ => Self::Hint,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARN",
            Self::Info => "INFO",
            Self::Hint => "HINT",
        }
    }
}

/// A diagnostic, with 1-based line/column for display (LSP is 0-based on the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

impl Diagnostic {
    /// `file:line:col [SEVERITY] code message (source)` — one line per diagnostic.
    pub fn display_line(&self) -> String {
        let code = self
            .code
            .as_deref()
            .map(|c| format!(" {c}"))
            .unwrap_or_default();
        let src = self
            .source
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        format!(
            "{}:{}:{} [{}]{} {}{}",
            self.file,
            self.line,
            self.column,
            self.severity.label(),
            code,
            self.message,
            src
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_order_error_first() {
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Hint);
        assert_eq!(DiagnosticSeverity::from_lsp(1), DiagnosticSeverity::Error);
        assert_eq!(DiagnosticSeverity::from_lsp(2), DiagnosticSeverity::Warning);
        assert_eq!(DiagnosticSeverity::from_lsp(99), DiagnosticSeverity::Hint);
    }

    #[test]
    fn display_line_formats() {
        let d = Diagnostic {
            file: "src/main.rs".into(),
            line: 10,
            column: 5,
            end_line: None,
            end_column: None,
            severity: DiagnosticSeverity::Error,
            message: "type mismatch".into(),
            source: Some("rustc".into()),
            code: Some("E0308".into()),
        };
        assert_eq!(
            d.display_line(),
            "src/main.rs:10:5 [ERROR] E0308 type mismatch (rustc)"
        );
        let d2 = Diagnostic {
            code: None,
            source: None,
            ..d
        };
        assert_eq!(d2.display_line(), "src/main.rs:10:5 [ERROR] type mismatch");
    }
}
