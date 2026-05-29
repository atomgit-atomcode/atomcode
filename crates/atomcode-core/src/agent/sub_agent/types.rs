//! Core type definitions for the sub-agent framework.
//!
//! Sub-agents are lightweight, scoped agents spawned by the main agent loop
//! for focused sub-tasks such as Q&A, lookups, or self-contained operations.


/// The kind of sub-agent to spawn.
#[derive(Debug, Clone, Copy)]
pub enum SubAgentKind {
    /// Simple question-and-answer agent.
    QnA,
}

/// Controls what tools a sub-agent has access to.
#[derive(Debug, Clone)]
pub enum SubAgentToolPolicy {
    /// No external tools — the agent can only use its knowledge base.
    None,
    /// Read-only filesystem tools (search, grep, read).
    ReadOnly,
    /// Read-only tools plus web-search / web-fetch.
    ReadOnlyWithWeb,
    /// An explicit, custom list of tool names.
    Custom(Vec<String>),
}

/// Full definition of a sub-agent, used at registration time.
#[derive(Debug, Clone)]
pub struct SubAgentDefinition {
    pub name: String,
    pub description: String,
    pub kind: SubAgentKind,
    pub system_prompt: String,
    pub model: Option<String>,
    pub tools: SubAgentToolPolicy,
    /// Knowledge base reference. Currently uses the `KnowledgeRef` (unit type)
    /// placeholder. The `guide::KnowledgeBase` type is fully implemented and
    /// standalone; wiring it as the actual type for this field happens in
    /// Task 10 (AgentLoop integration).
    pub knowledge: Option<KnowledgeRef>,
    pub max_turns: usize,
    pub max_answer_tokens: usize,
    pub max_knowledge_tokens: usize,
    pub compression_threshold: f64,
}

impl Default for SubAgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            kind: SubAgentKind::QnA,
            system_prompt: String::new(),
            model: None,
            tools: SubAgentToolPolicy::ReadOnlyWithWeb,
            knowledge: None,
            max_turns: 8,
            max_answer_tokens: 1200, // ~2400 chars at 2 chars/token
            max_knowledge_tokens: 8_000,
            compression_threshold: 0.9,
        }
    }
}

/// Result produced by a completed sub-agent invocation.
#[derive(Debug, Clone)]
pub struct SubAgentOutput {
    pub text: String,
    pub truncated: bool,
}

/// Error produced by a sub-agent that failed (e.g. hit turn limit, LLM error).
#[derive(Debug, Clone)]
pub struct SubAgentError {
    pub turns_used: usize,
    pub message: String,
    pub cancelled: bool,
}

impl std::fmt::Display for SubAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SubAgentError ({} turns): {}",
            self.turns_used, self.message
        )
    }
}

impl std::error::Error for SubAgentError {}

/// Knowledge provider trait — renders relevant knowledge content for a query.
/// Implemented by `guide::KnowledgeBase` and wired at registration time.
pub trait KnowledgeProvider: Send + Sync {
    fn render_for_query(&self, query: &str, max_tokens: usize) -> String;
}

impl std::fmt::Debug for dyn KnowledgeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KnowledgeProvider")
    }
}

/// Placeholder type for KnowledgeBase reference. Will be replaced in Task 5.
pub type KnowledgeRef = std::sync::Arc<dyn KnowledgeProvider>;

/// Convenience alias for the result type used throughout the sub-agent API.
pub type SubAgentOutcome = Result<SubAgentOutput, SubAgentError>;
