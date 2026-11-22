//! CUE — the provider-agnostic half of AI inline completion.
//!
//! "CUE" is the completion engine: the model proposes the text that belongs at
//! the cursor, and the editor shows it as ghost text (the TRAE / Copilot
//! inline-completion shape).
//!
//! This module owns the part that does not care WHICH model answers:
//!
//! - **building the request** — the surrounding code becomes a bounded, fenced
//!   prompt ([`prompt`]);
//! - **reading the answer** — the model's reply becomes the exact text to
//!   insert ([`extract`]).
//!
//! Both halves live at L1 (kernel-only) on purpose: the VSCode extension and
//! the JetBrains plugin ask for completions over different transports, but
//! neither should own the prompt. A driver that re-implemented the wording
//! would drift from the other one, and the drift would be invisible until a
//! user's completion came back subtly wrong.
//!
//! The transport and the model choice are the driver's business: the daemon
//! resolves the provider (it holds the credentials) and calls this module to
//! shape the result.
//!
//! ```
//! use atomcode_capabilities::cue::{build_completion_messages, extract_completion, CompletionContext};
//!
//! let context = CompletionContext {
//!     file_path: "src/main.rs".into(),
//!     language_id: "rust".into(),
//!     prefix: "fn main() {\n    pri".into(),
//!     suffix: "();\n}".into(),
//! };
//! let messages = build_completion_messages(&context);
//! assert_eq!(messages.len(), 2);
//! assert!(messages[1].text.contains("fn main()"));
//!
//! assert_eq!(extract_completion("ntln!(\"hi\");").as_deref(), Some("ntln!(\"hi\");"));
//! assert_eq!(extract_completion("<<<NO_COMPLETION>>>"), None);
//! ```

mod extract;
mod prompt;

pub use extract::{extract_completion, NO_COMPLETION_SENTINEL};
pub use prompt::{
    build_completion_messages, CompletionContext, MAX_PREFIX_CHARS, MAX_SUFFIX_CHARS,
};
