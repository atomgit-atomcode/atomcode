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
    /// Vue SFC — script section extracted and parsed as TypeScript.
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
            Lang::Vue => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }

    /// The tree-sitter query for extracting function/method definitions.
    /// Returns (query_string, function_capture_name, name_capture_name).
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
            Lang::Vue => include_str!("queries/typescript.scm"),
        }
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
            "vue" | "svelte" => Some(Lang::Vue),
            _ => None,
        }
    }
}
