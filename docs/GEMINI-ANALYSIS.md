# Gemini Architecture Suggestion: Comparative Analysis

**Date:** 2026-03-19
**Author:** Chief Architect
**Context:** Gemini provided an independent architecture suggestion without seeing our refactoring plan. This document compares their proposal against our existing plan and current architecture.

---

## 1. What Gemini Gets Right (That We Should Adopt)

### 1.1 `get_file_outline(path)` -- AST-based code outline

**Already in our plan?** No. Our plan adds `list_directory`, `grep_search`, and `glob` tools (Section 4.2), but we have no tool that extracts a structural outline of a single file without sending its full content.

**Should it be?** Yes, absolutely. This is one of the genuinely valuable ideas in Gemini's proposal. Consider the use case: the LLM needs to understand a 2000-line file to make a targeted edit. Today it must `read_file` the whole thing (eating 500+ tokens of context) or guess. A `get_file_outline` tool that returns function signatures, struct/class definitions, and import statements would let the LLM decide *which part* to read. This is directly aligned with our CLAUDE.md principle: "implement outline extraction mechanisms for large files."

Our ARCHITECTURE.md even flags this gap (Section 12, Principle 4): "No outline extraction for large files beyond line limits."

**Implementation note:** Full AST parsing (tree-sitter) is the right approach but adds a significant dependency. A pragmatic V1 could use regex-based extraction for common patterns (function/class/struct/impl/def/fn declarations) -- imperfect but useful. This stays tech-stack agnostic because the tool itself is generic; the regex patterns just happen to match common declaration syntax across languages.

**Verdict: Add to our plan as a Phase 5 tool (alongside grep_search and glob).**

### 1.2 Dynamic log/output truncation (first + last lines)

**Already in our plan?** Partially. Our architecture has a fixed 8000-character truncation with `[truncated...]` appended (ARCHITECTURE.md Section 6.4). But the truncation is dumb -- it just cuts at 8000 chars.

**Should it be improved?** Yes. Gemini's suggestion to show the first N and last N lines of huge outputs is strictly better than our current "cut after 8000 chars" approach. When a build fails, the error is usually at the *end* of the output. When a test suite runs, the summary is at the end. Our current truncation discards exactly the part the LLM needs most.

**Implementation:** Replace the current `truncate_output()` with a head+tail strategy:

```
if output.lines().count() > MAX_LINES:
    show first 20 lines
    "[... {N} lines truncated ...]"
    show last 30 lines
```

This is a small change with high impact. It can be done immediately, independent of any phase.

**Verdict: Add as an immediate improvement (pre-Phase 1). Trivial to implement, high value.**

### 1.3 Explicit Thought/Action/Observation phases in the loop

**Already in our plan?** Partially. Our `AgentLoop::agent_turn()` (Section 1.1) has an explicit loop with clear phases: call provider -> drain stream -> handle outcome (ToolCall/Done/Error). But we do not label or surface the Thought/Action/Observation distinction to the user or the LLM.

**Should it be?** The ReAct pattern's Thought phase is interesting but must be handled carefully. Some models (Claude, GPT-4) naturally emit "thinking" text before tool calls. Making this *explicit* in the loop structure (rather than just letting the model do it) would mean either:
- (a) Forcing a two-step generation (first generate thought, then generate action) -- doubles API calls, bad for latency.
- (b) Parsing the model's output to separate thought from action -- fragile and model-dependent.
- (c) Just labeling the existing phases in the UI -- this is what we partially do already (tool calls are cyan, results are green/red).

Option (c) is the right move. Our CLAUDE.md says: "use clear colors to distinguish Thought, Action, and Response." Our architecture already does this visually but does not have a formal `Phase` enum in the agent loop.

**Verdict: Not a structural change. Improve UI labeling of phases (already tracked as a polish item). Do not add a separate "thought generation" step.**

### 1.4 Safety interceptor (Router) BEFORE the ReAct loop

**Already in our plan?** Yes, more comprehensively. Our `PermissionStore` (Section 1.4) with four permission levels (AlwaysAllow/Ask/ToolDefault/AlwaysDeny), per-tool overrides, session grants, and config integration is significantly more detailed than Gemini's "safety interceptor" concept.

However, Gemini's framing of a "Router" that handles intent classification *before* entering the heavy loop is a slightly different idea. The question is: should we do a lightweight pre-check (is this request safe/appropriate?) before committing to the full agent loop? Currently, we send every user message directly to the full provider call with all tools available.

**Should it be?** For a terminal coding agent, the pre-routing adds latency for minimal benefit. The LLM itself is the best "router" -- it decides whether to use tools or just respond with text. A separate router would either be a second LLM call (expensive) or a regex/keyword matcher (brittle). Our permission system handles the safety concern at the tool-execution level, which is the right place.

**Verdict: No. Our permission system is superior. A pre-loop router adds latency without proportional safety gains for a single-user terminal tool.**

---

## 2. What Gemini Gets Wrong or Oversimplifies

### 2.1 "Four Modules" is architecturally naive

Gemini proposes four modules: Brain Layer, Action Space, Observation Layer, Context Window Management. This is a conceptual taxonomy, not an architecture. It does not address:

- **How do modules communicate?** No mention of channels, events, or async boundaries.
- **Where does state live?** No conversation struct, no message types, no serialization format.
- **How does the UI fit?** No mention of TUI, rendering, input handling, or the fundamental question of how a terminal app processes both user input and async LLM responses.
- **How are providers abstracted?** "LLM Router & Reasoner" conflates the provider abstraction (how to talk to OpenAI vs Claude vs Ollama) with the decision-making logic (what to do with the response).

Our architecture has 12 detailed sections covering all of these. Gemini's is a whiteboard sketch.

### 2.2 "Sense tools" vs "Manipulate tools" is a false dichotomy

Gemini categorizes tools into:
- Sense: `search_codebase`, `list_directory`, `get_file_outline`
- Manipulate: `read_file`, `edit_file`, `execute_bash`

This categorization is wrong. `read_file` is a *sense* tool (it reads, it doesn't modify anything). `execute_bash` can be either sense (`ls`, `grep`) or manipulate (`rm`, `gcc`). The tool system should not care about this taxonomy -- it is the *permission system* that cares about whether an operation is destructive, and it makes that determination per-invocation based on the arguments, not per-tool based on a static category.

Our `Tool::approval(args)` method does this correctly: it inspects the actual arguments (e.g., does this bash command contain `rm -rf`?) rather than assigning tools to fixed categories.

### 2.3 "Sliding window + summarization" is stated but not designed

Gemini says "sliding window + summarization when approaching token limit" as if it is a single bullet point. Our plan has:
- A `TokenBudget` struct with model-specific context windows (Section 2.3)
- A character-based token estimator with CJK awareness (Section 2.3)
- Token-aware message windowing that walks backwards from recent messages (Section 2.3)
- A `ConversationSummarizer` with a concrete implementation (Section 3.1)
- A two-phase approach: rule-based summarization first, LLM-powered summarization later (Section 3.1)
- Injection strategy for summaries (first user message after system prompt) (Section 3.1)

Gemini's version is aspirational. Ours is implementable.

### 2.4 No mention of cancellation, error handling, or retry

Gemini's proposal has zero mention of:
- What happens when the user presses Ctrl+C during a tool execution
- What happens when an API call fails (429, network error, timeout)
- How to abort an HTTP stream mid-flight
- How to kill a child process spawned by a bash tool

These are not edge cases. They are the difference between a toy demo and a usable tool. Our plan dedicates an entire section (1.3) to `CancellationToken` propagation with concrete code for every scenario.

### 2.5 No mention of provider differences

Gemini says "LLM Router" without acknowledging that:
- OpenAI uses `tools` parameter with `function` type and SSE streaming
- Claude uses `tools` with `tool_use` content blocks and a different SSE format
- Ollama uses NDJSON streaming and a different API shape entirely

Our plan has detailed implementations for each (Sections 2.1-2.2) including the exact wire formats, streaming parsers, and message formatting differences.

### 2.6 "State machine: Thought -> Action -> Observation -> loop" is incomplete

A real agent state machine has more states than this. Ours (ARCHITECTURE.md Section 5.1):
- Normal -> Streaming -> ToolExecuting -> Streaming -> ... -> Normal
- Streaming -> WaitingApproval -> ToolExecuting (if approved) / Normal (if denied)
- Error states, retry states, cancellation states

Gemini's three-state loop does not handle: user approval, cancellation, error recovery, retry, multi-tool-call fan-out, or the transition back to idle when the LLM is done.

---

## 3. What Our Plan Has That Gemini Missed

### 3.1 God Object decomposition with channel-based communication

Our plan's core contribution is the `AgentLoop` / `App` split (Section 1.1) with `AgentCommand` / `AgentEvent` channels. This is the single most important architectural decision and Gemini does not address it at all. The separation enables:
- Headless mode (no TUI)
- Testable agent logic
- Clean ownership boundaries
- Future web/API frontends

### 3.2 ToolContext for shared mutable state

Our `ToolContext` with `Arc<RwLock<PathBuf>>` for working directory (Section 1.2) solves the BashTool/CdTool coordination problem. Gemini's tool registry is a simple lookup table with no concept of shared execution context.

### 3.3 Permission system with session grants

Our four-level permission model with session-level "always allow" grants (Section 1.4) is significantly more sophisticated than Gemini's "safety interceptor." Users who trust bash can press "A" once and never be prompted again for the session. Users who want to lock down write_file can set it to AlwaysDeny in config.

### 3.4 Multi-tool parallel execution

Our plan addresses parallel tool calls (Section 2.2) with concrete changes to `StreamEvent::ToolCallDone(Vec<ToolCall>)` and `tokio::spawn` for concurrent execution. Gemini does not mention parallel tool calls.

### 3.5 Claude native tool_use implementation

Section 2.1 of our plan has the complete Claude content-block format, including streaming `input_json_delta` parsing. Gemini's "LLM Router" concept does not distinguish between providers.

### 3.6 Migration strategy with phased rollout

Our plan has a six-phase migration strategy (Section 6) with dependency graphs, risk assessments, and duration estimates. Gemini's proposal is a greenfield design with no migration path from the current codebase.

### 3.7 External tool plugin system

Our plan includes a TOML-based tool manifest format and `ExternalTool` wrapper (Section 4.1) for tools implemented as separate binaries. Gemini's "Tool Registry" is hardcoded.

---

## 4. Concrete Additions to Our Refactoring Plan

Based on this analysis, the following items should be ADDED:

### Addition 1: `get_file_outline` tool (Phase 5c)

Add to Section 4.2 alongside GrepSearchTool and GlobTool:

```
4.2.4 FileOutlineTool

A tool that returns the structural outline of a source file: function signatures,
class/struct/enum definitions, impl blocks, imports. Does NOT return function bodies.

V1 implementation: regex-based extraction for common patterns across languages
(fn, def, class, struct, enum, interface, func, function, pub, export, import, use).

V2 implementation: tree-sitter integration for AST-accurate outlines.

Parameters:
- path (string, required): file path
- detail_level (string, optional): "signatures" (default) or "full_declarations"

This directly addresses the CLAUDE.md requirement for "outline extraction" and reduces
context consumption when the LLM needs to understand file structure without reading
every line.
```

### Addition 2: Smart output truncation (head + tail) (Pre-Phase 1)

Replace the current fixed-cutoff truncation in tool output with a head+tail strategy:

```
Current (ARCHITECTURE.md 6.4):
  Tool outputs exceeding 8000 characters are truncated with [truncated...].

New:
  Tool outputs exceeding MAX_OUTPUT_LINES (default 200) are truncated to:
  - First 30 lines (often contain the command echo and initial output)
  - "[... N lines omitted ...]"
  - Last 50 lines (contain error messages, summaries, final status)

  The character limit (8000) is applied AFTER the line-based truncation.
```

This is a trivial change to the existing `truncate_output()` function and can be done before any phase begins.

### Addition 3: Observation formatting in AgentEvent (Phase 4)

When implementing `AgentEvent`, add an `ObservationFormatted` variant or ensure that tool results sent to the UI include explicit phase labeling:

```rust
pub enum AgentEvent {
    // ... existing variants ...
    PhaseChange(AgentPhase),  // NEW
}

pub enum AgentPhase {
    Thinking,      // LLM is generating text (before any tool call)
    Acting(String), // Executing tool (with tool name)
    Observing,     // Processing tool result
    Responding,    // LLM generating final response
}
```

This enables the UI to show explicit phase indicators, satisfying the CLAUDE.md requirement for "color-coded Thought/Action/Response" distinction. Low cost, improves UX.

### Addition 4: Document the "no pre-loop router" decision (Architecture doc)

Add an explicit ADR (Architecture Decision Record) noting that we evaluated and rejected a pre-loop intent/safety router in favor of per-tool-execution permission checks. Reasons:
- Adds latency (either a second LLM call or brittle heuristics)
- Permission checks at execution time are more precise (inspect actual arguments)
- Single-user terminal tool does not need the same safety layering as a multi-tenant API

This prevents future contributors from re-proposing the same idea.

---

## 5. Revised Priority Order

The original plan's phasing is sound. The additions from this analysis slot in naturally. Here is the revised order:

### Immediate (before Phase 1)
- **Smart output truncation (head + tail)** -- 30 minutes of work, immediate quality-of-life improvement for every agent interaction. Do this first.

### Phase 1: Foundation (1-2 weeks, unchanged)
1a. ToolContext
1b. Fix BashTool
1c. Fix CdTool
1d. CancellationToken
1e. PermissionStore

### Phase 2: Claude Provider (1 week, unchanged, parallel with Phase 1)
2a-2d as specified.

### Phase 3: Multi-tool-call support (3-4 days, unchanged, parallel with Phase 1)
3a-3c as specified.

### Phase 4: Extract AgentLoop (2-3 weeks)
4a-4e as specified, **plus:**
- 4f. Add `AgentPhase` enum and `PhaseChange` event (Addition 3 above)

### Phase 5: Context & Tools (1 week)
5a. Token budget management
5b. Context summarization
5c. On-demand project context tools: `list_directory`, `grep_search`, `glob`, **`file_outline`** (Addition 1)

### Phase 6: Polish (ongoing, unchanged)
6a-6f as specified.

### Net assessment of priority changes:
- Output truncation improvement moves to "do it now" status.
- `file_outline` tool is added to Phase 5c.
- `AgentPhase` is added to Phase 4.
- No existing items change priority. The original ordering is correct.

---

## Summary

Gemini's proposal is a reasonable high-level sketch of a ReAct agent. It identifies the right conceptual pieces (loop, tools, context management) and contributes two genuinely useful ideas: file outlines and smart output truncation. However, it is an order of magnitude less detailed than our refactoring plan. It does not address the hard engineering problems: provider differences, cancellation, error recovery, state management, UI integration, or migration from an existing codebase.

The honest comparison: Gemini described *what* an agent should do. Our plan describes *how* to build one, starting from the codebase we actually have.

**Adopted from Gemini:** 2 ideas (file outline tool, head+tail truncation).
**Rejected from Gemini:** 3 ideas (pre-loop router, sense/manipulate taxonomy, explicit Thought generation step).
**Confirmed by Gemini:** Our existing plan is on the right track -- the convergence on tools like grep search and list directory validates our direction.
