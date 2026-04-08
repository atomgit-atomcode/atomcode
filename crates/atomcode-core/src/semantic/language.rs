use std::path::Path;
use tree_sitter::Language;

/// Supported languages with their tree-sitter grammars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    Html,
    /// Vue SFC — dual parser: <script> as TypeScript, <template> as HTML.
    Vue,
}

impl Lang {
    /// Get the tree-sitter Language grammar for this language.
    pub fn grammar(&self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Html => tree_sitter_html::LANGUAGE.into(),
            Lang::Vue => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }

    /// The tree-sitter query for extracting function/method definitions.
    pub fn symbols_query(&self) -> &'static str {
        match self {
            Lang::Rust => include_str!("queries/rust.scm"),
            Lang::Python => include_str!("queries/python.scm"),
            Lang::JavaScript | Lang::Tsx => include_str!("queries/javascript.scm"),
            Lang::TypeScript => include_str!("queries/typescript.scm"),
            Lang::Go => include_str!("queries/go.scm"),
            Lang::Java => include_str!("queries/java.scm"),
            Lang::C => include_str!("queries/c.scm"),
            Lang::Cpp => include_str!("queries/cpp.scm"),
            Lang::Html => include_str!("queries/html.scm"),
            Lang::Vue => include_str!("queries/typescript.scm"),
        }
    }

    /// The tree-sitter query for extracting call expressions (callee names).
    pub fn calls_query(&self) -> Option<&'static str> {
        match self {
            Lang::Rust => Some(include_str!("../graph/queries/rust_calls.scm")),
            Lang::Python => Some(include_str!("../graph/queries/python_calls.scm")),
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Vue => {
                Some(include_str!("../graph/queries/javascript_calls.scm"))
            }
            Lang::Java => Some(include_str!("../graph/queries/java_calls.scm")),
            Lang::Go => Some(include_str!("../graph/queries/go_calls.scm")),
            Lang::C | Lang::Cpp => None,
            Lang::Html => None,
        }
    }

    /// Whether this language is a Vue SFC (needs dual parsing).
    pub fn is_vue(&self) -> bool {
        matches!(self, Lang::Vue)
    }

    /// Get the HTML grammar for parsing Vue <template> sections.
    pub fn html_grammar() -> Language {
        tree_sitter_html::LANGUAGE.into()
    }
}

/// Registry that maps file extensions to languages.
pub struct LanguageRegistry;

impl LanguageRegistry {
    /// Detect language from file path extension.
    pub fn detect(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        match ext {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "js" | "mjs" | "cjs" => Some(Lang::JavaScript),
            "ts" | "mts" => Some(Lang::TypeScript),
            "tsx" | "jsx" => Some(Lang::Tsx),
            "go" => Some(Lang::Go),
            "java" => Some(Lang::Java),
            "c" | "h" => Some(Lang::C),
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some(Lang::Cpp),
            "html" | "htm" => Some(Lang::Html),
            "vue" | "svelte" => Some(Lang::Vue),
            _ => None,
        }
    }
}
