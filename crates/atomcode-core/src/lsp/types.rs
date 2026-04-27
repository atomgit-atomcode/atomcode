use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

impl DiagnosticSeverity {
    pub fn from_lsp(value: u32) -> Self {
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
    pub fn display_line(&self) -> String {
        let sev = self.severity.label();
        let code = self
            .code
            .as_deref()
            .map(|c| format!(" {}", c))
            .unwrap_or_default();
        let src = self
            .source
            .as_deref()
            .map(|s| format!(" ({})", s))
            .unwrap_or_default();
        format!(
            "{}:{}:{} [{}]{} {}{}",
            self.file, self.line, self.column, sev, code, self.message, src
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_display_line() {
        let diag = Diagnostic {
            file: "src/main.rs".into(),
            line: 10,
            column: 5,
            end_line: Some(10),
            end_column: Some(15),
            severity: DiagnosticSeverity::Error,
            message: "unused variable".into(),
            source: Some("rust-analyzer".into()),
            code: Some("E0001".into()),
        };
        assert_eq!(
            diag.display_line(),
            "src/main.rs:10:5 [ERROR] E0001 unused variable (rust-analyzer)"
        );

        // Without code and source
        let diag2 = Diagnostic {
            file: "lib.rs".into(),
            line: 1,
            column: 1,
            end_line: None,
            end_column: None,
            severity: DiagnosticSeverity::Warning,
            message: "deprecated function".into(),
            source: None,
            code: None,
        };
        assert_eq!(
            diag2.display_line(),
            "lib.rs:1:1 [WARN] deprecated function"
        );
    }

    #[test]
    fn severity_ordering() {
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Info);
        assert!(DiagnosticSeverity::Info < DiagnosticSeverity::Hint);
    }

    #[test]
    fn severity_from_lsp() {
        assert_eq!(DiagnosticSeverity::from_lsp(1), DiagnosticSeverity::Error);
        assert_eq!(DiagnosticSeverity::from_lsp(2), DiagnosticSeverity::Warning);
        assert_eq!(DiagnosticSeverity::from_lsp(3), DiagnosticSeverity::Info);
        assert_eq!(DiagnosticSeverity::from_lsp(4), DiagnosticSeverity::Hint);
        // Unknown values default to Hint
        assert_eq!(DiagnosticSeverity::from_lsp(99), DiagnosticSeverity::Hint);
    }
}
