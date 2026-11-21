//! Small, offline capability catalog. Detection, installation and UI belong to hosts.

use super::Stack;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    AstGrep,
    Context7,
    NativeLsp { extension: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recommendation {
    pub id: &'static str,
    /// Bump only when this recommendation changes enough to warrant a new notice.
    pub revision: u32,
    pub name: &'static str,
    pub capability: Capability,
}

pub const CONTEXT7_URL: &str = "https://mcp.context7.com/mcp";

pub fn recommendations_for(stacks: &[Stack]) -> Vec<Recommendation> {
    let mut result = vec![Recommendation {
        id: "ast-grep",
        revision: 1,
        name: "ast-grep",
        capability: Capability::AstGrep,
    }];
    if !stacks.is_empty() {
        result.push(Recommendation {
            id: "context7",
            revision: 1,
            name: "Context7",
            capability: Capability::Context7,
        });
    }
    for stack in stacks {
        let (id, name, extension) = match stack {
            Stack::Rust => ("lsp-rust", "Rust LSP", "rs"),
            Stack::Python => ("lsp-python", "Python LSP", "py"),
            Stack::Node => ("lsp-node", "JavaScript / TypeScript LSP", "ts"),
            Stack::Java => ("lsp-java", "Java LSP", "java"),
            Stack::Go => ("lsp-go", "Go LSP", "go"),
        };
        if !result.iter().any(|r| r.id == id) {
            result.push(Recommendation {
                id,
                revision: 1,
                name,
                capability: Capability::NativeLsp { extension },
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_recommend_existing_native_capabilities_without_duplicate_servers() {
        let rust = recommendations_for(&[Stack::Rust, Stack::Rust]);
        assert_eq!(
            rust.iter().map(|r| r.id).collect::<Vec<_>>(),
            ["ast-grep", "context7", "lsp-rust"]
        );
        let python = recommendations_for(&[Stack::Python]);
        assert!(python
            .iter()
            .any(|r| r.capability == Capability::NativeLsp { extension: "py" }));
        assert_eq!(recommendations_for(&[]).len(), 1);
        assert_eq!(recommendations_for(&[Stack::Rust, Stack::Node]).len(), 4);
    }
}
