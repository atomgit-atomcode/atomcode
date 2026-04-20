# Context Manager Refactor — Design Spec

**Date:** 2026-04-20
**Branch target:** new feature branch off `main`
**Status:** Draft, pending user review

## 1. Motivation

Current context management code is scattered across four files with hardcoded
thresholds tuned for Claude-class 128K windows:

- `conversation/mod.rs` (1952 lines) — `to_provider_messages_budgeted`,
  `needs_compression`, `build_compression_content`, `apply_compression`,
  `microcompact`, `replace_stale_reads`, cold zone state.
- `agent/mod.rs` — `maybe_compress_history` invokes the default provider
  to summarize and injects post-compression state.
- `turn/truncation.rs` (521 lines) — per-tool + universal tool output
  truncation with `hard_char_limit = ctx/8`.
- `config/provider.rs` — per-provider `context_window`.

Problems:

1. **Thresholds (compress at 50%, hard cut at 80%, `KEEP_MESSAGES=20`,
   `keep_recent=5`, cold zone cap=3) are baked in.** Small local models
   (8K–32K) overflow before compression triggers; very large windows
   (≥ 200K Gemini, Claude 1M) compress far earlier than necessary.
2. **No model-specific hooks.** Claude's prompt cache breakpoints,
   thinking block preservation, and per-provider tool-result handling
   have nowhere to live.
3. **Tool output truncation uses the same budget for every provider.**
   Small windows should cut more aggressively.

Primary driver for this refactor is **window-size adaptivity**. Model-specific
capability hooks (prompt cache, thinking blocks) and per-strategy tool
truncation budgets ride along. Switching the summarizer to a cheaper
sub-model is **out of scope** for this iteration but the design leaves
the extension point open.

## 2. Scope

**In scope** — all of the following moves into a new `context/` module:

- `conversation/mod.rs`: `to_provider_messages_budgeted`, `needs_compression`,
  `build_compression_content`, `microcompact`, `replace_stale_reads`,
  `clean_message_pipeline`.
- `agent/mod.rs`: the dispatch in `maybe_compress_history` that calls
  `needs_compression` / `build_compression_content` / `apply_compression`.
  The LLM invocation and post-compression state injection stay in `agent`.
- `turn/truncation.rs`: everything (`truncate_output`, `UNIVERSAL_MAX_LINES`,
  `hard_char_limit`, `turn_budget`).

**Out of scope — explicitly preserved as-is:**

- `Conversation` as a data container (`messages`, `turn_tracker`,
  `cold_summaries`, `apply_compression`, `add_user_message`, etc.).
- `config/provider.rs` fields (`context_window`, `max_tokens`).
- LLM call in `maybe_compress_history` and post-compression state
  restoration in `agent`.
- The turn_tracker invariants enforced by `apply_compression`.

## 3. Keying: how strategies map to models

Two-stage resolution inside `context::resolve_strategy(model_id, ctx_window)`:

1. **Model ID rule table (prefix match).** Only families with a
   demonstrable shape difference get an entry. Initial entries:
   - `claude-*` → `ClaudeStrategy` wrapping the size-tier strategy
     (Medium for ≤200K, Large for >200K).
2. **Size-tier fallback** when no rule hits:
   - `ctx_window ≤ 32_000` → `SmallWindowStrategy`
   - `32_001..=200_000` → `MediumWindowStrategy`
   - `> 200_000` → `LargeWindowStrategy`

OpenAI `gpt-*`, Ollama `llama-*`, and any custom model go through the
size tier alone — zero config required.

## 4. Architecture

```
crates/atomcode-core/src/
├── context/                      ← new module
│   ├── mod.rs                    ← re-exports + resolve_strategy()
│   ├── strategy.rs               ← ContextStrategy trait, Capabilities,
│   │                               RenderedContext, CompressionPlan,
│   │                               ContextStats
│   ├── resolver.rs               ← model_id rules + size-tier fallback
│   ├── small.rs                  ← SmallWindowStrategy (<32K)
│   ├── medium.rs                 ← MediumWindowStrategy (32K–200K,
│   │                               behavior-equivalent to current code)
│   ├── large.rs                  ← LargeWindowStrategy (>200K)
│   ├── claude.rs                 ← ClaudeStrategy composes inner strategy
│   │                               and adds prompt-cache breakpoints
│   ├── render.rs                 ← shared helpers: microcompact,
│   │                               replace_stale_reads, clean_pipeline,
│   │                               drop_digest, absolute floor
│   ├── truncate.rs               ← migrated from turn/truncation.rs
│   └── tests.rs                  ← strategy_contract_suite! macro +
│                                   snapshot regression tests
│
├── conversation/                 ← slimmed
│   ├── mod.rs                    ← data container only: messages,
│   │                               turn_tracker, cold_summaries,
│   │                               add_user_message, apply_compression
│   ├── message.rs                ← unchanged
│   └── turn.rs                   ← unchanged
│
├── agent/mod.rs                  ← holds Box<dyn ContextStrategy>;
│                                   maybe_compress_history delegates to
│                                   strategy.compression_plan()
└── turn/truncation.rs            ← deleted
```

Key relationships:

- `AgentLoop` holds one `Box<dyn ContextStrategy>` decided at construction
  from `(model_id, context_window)`.
- `Conversation` is borrowed: `&Conversation` for render, `&mut Conversation`
  for `apply_compression` (which remains a method on `Conversation`).
- Shared helpers in `render.rs` are called by concrete strategies as
  composition, not inheritance — Rust has no inheritance.
- `ClaudeStrategy` holds an inner `Box<dyn ContextStrategy>` (Medium or
  Large), delegates all methods, and only overrides `render` to compute
  cache breakpoints.

## 5. The `ContextStrategy` trait

```rust
pub trait ContextStrategy: Send + Sync {
    fn render(&self, conv: &Conversation, system_prompt: &str) -> RenderedContext;

    fn should_compress(&self, conv: &Conversation, system_tokens: usize) -> bool;

    fn compression_plan(&self, conv: &Conversation) -> Option<CompressionPlan>;

    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str);

    fn capabilities(&self) -> &Capabilities;

    fn name(&self) -> &'static str;
}

pub struct RenderedContext {
    pub messages: Vec<Message>,
    pub stats: ContextStats,           // system/sent/dropped tokens, msg_count
    pub cache_breakpoints: Vec<usize>, // Claude only; other strategies empty
}

pub struct CompressionPlan {
    pub content_to_summarize: String,
    pub messages_to_remove: usize,     // passed to Conversation::apply_compression
    pub keep_recent_messages: usize,
}

#[derive(Default, Clone)]
pub struct Capabilities {
    pub prompt_cache: bool,
    pub preserve_thinking_blocks: bool,
    pub tool_result_in_cache: bool,
    pub max_output_tokens: Option<usize>,
}
```

Design rules:

- **Stateless strategies.** Implementations hold only constant config
  (thresholds, `ctx_window`). No cross-turn caching.
- **No I/O.** `render` and `compression_plan` are pure functions. No
  file reads, no LLM calls, no datalog access.
- **Strategy doesn't call LLM.** `compression_plan` returns content
  to summarize; `agent::maybe_compress_history` runs the LLM and calls
  `Conversation::apply_compression`. This keeps the summarizer
  swap-out (out-of-scope item 4) as a future single-point change.
- **`apply_compression` stays on `Conversation`.** It's a data mutation
  with turn_tracker invariants; strategies compute the plan, data
  applies it.
- **State restoration stays in `agent`.** `current_task`,
  `files_edited_this_turn`, `files_read_this_turn` injection is runtime
  state, unrelated to strategy.

## 6. Strategy parameters

| Strategy | Window | compress_at | hard_cut_at | keep_recent | cold_max | tool `hard_char_limit` |
|---|---|---|---|---|---|---|
| `SmallWindowStrategy`  | <32K     | 50% | 70% | 3 turns | 2 entries | `ctx/10`, clamped `[4K, 12K]` |
| `MediumWindowStrategy` | 32K–200K | 50% | 80% | 5 turns | 3 entries | `ctx/8`,  clamped `[8K, 32K]` ← current code |
| `LargeWindowStrategy`  | >200K    | 60% | 85% | 8 turns | 5 entries | `ctx/8`,  clamped `[16K, 64K]` |
| `ClaudeStrategy`       | delegates | — | — | — | — | — |

**Medium is the current behavior unchanged.** This refactor must not
alter the existing Claude/OpenAI paths that run at 128K; Small and
Large are the new branches.

`ClaudeStrategy` is composition:

```rust
pub struct ClaudeStrategy {
    inner: Box<dyn ContextStrategy>,
    caps: Capabilities, // prompt_cache=true, preserve_thinking_blocks=true
}
impl ContextStrategy for ClaudeStrategy {
    fn render(&self, conv: &Conversation, sys: &str) -> RenderedContext {
        let mut r = self.inner.render(conv, sys);
        r.cache_breakpoints = self.compute_cache_breakpoints(&r.messages);
        r
    }
    // should_compress, compression_plan, truncate_tool_output: delegate to inner
    fn capabilities(&self) -> &Capabilities { &self.caps }
    fn name(&self) -> &'static str { "claude" }
}
```

## 7. Data flow

### 7.1 Initialization

```
AgentLoop::new(config)
  └─ provider_cfg = config.providers[config.default_provider]
  └─ self.context = resolve_strategy(&provider_cfg.model,
                                     provider_cfg.context_window)
```

### 7.2 Per-turn render

```
agent.run_turn()
  ├─ rendered = self.context.render(&self.conversation, &system_prompt)
  ├─ turn_runner.run(rendered.messages, rendered.cache_breakpoints)
  │      (cache_breakpoints empty → provider ignores)
  └─ on tool result:
        self.context.truncate_tool_output(&mut result, tool_name)
```

### 7.3 Compression (end of turn)

```
agent.after_turn()
  ├─ if self.context.should_compress(&self.conversation, sys_tokens):
  │     plan = self.context.compression_plan(&self.conversation)
  │     summary = call_llm_to_summarize(&plan.content_to_summarize)
  │     if summary.is_empty(): summary = plan.content_to_summarize
  │     self.conversation.apply_compression(plan.messages_to_remove, summary)
  │     inject_recovery_state(...)  // current_task, files_edited — stays in agent
```

## 8. Error handling and invariants

### 8.1 Unknown `model_id`, missing or degenerate `context_window`

| Case | Handling |
|---|---|
| `model_id` matches no rule | `resolve_by_window(ctx)`, silent |
| `context_window == 0` or field absent | use `ctx.max(8000)`, SmallWindow; `tracing::warn!` once at startup |
| `context_window` > 200K (Gemini, Claude 1M) | LargeWindow |
| Provider config entirely missing | keep current `unwrap_or(128000)` fallback → Medium |

### 8.2 LLM summarization failure

Unchanged from current behavior: empty summary → fall back to the
mechanical one-liners from `plan.content_to_summarize`. This logic
stays in `agent::maybe_compress_history`.

### 8.3 Render invariants (every strategy must satisfy)

1. At least one non-System message when `conversation.messages` is non-empty
   (absolute floor).
2. The last turn is never dropped (HARD FLOOR).
3. Outgoing message sequence is provider-legal — `tool_call` and
   `tool_result` paired, no empty `AssistantWithToolCalls`.
4. `stats.sent_tokens <= token_budget` after any necessary hard cut.

These are enforced via a shared contract test suite (§ 10).

### 8.4 `apply_compression` invariants

The existing `CRITICAL INVARIANT` block in `Conversation::apply_compression`
(six assertions around `start_idx < new_len`, `end_idx <= new_len`,
`msg_count > 0`) is preserved verbatim. It is a data-layer concern,
not a strategy concern.

## 9. Capabilities — populated but not read this iteration

`Capabilities` fields are filled by each strategy (Claude sets
`prompt_cache=true` and `preserve_thinking_blocks=true`; others leave
defaults) but the provider layer does **not** read them in this
refactor. Actual prompt-cache wiring (emitting `cache_control: ephemeral`
in the Claude payload) is a follow-up PR. This keeps the current
refactor's surface area minimal and the regression window small.

## 10. Testing strategy

| Layer | File(s) | What |
|---|---|---|
| Shared invariants | `context/tests.rs` — `strategy_contract_suite!` macro | The 4 invariants in § 8.3, run once per strategy |
| Strategy-specific | `small.rs`, `medium.rs`, `large.rs`, `claude.rs` `#[cfg(test)]` | Threshold-specific behavior: Small's 50% compress, Large's `keep_recent=8`, Claude cache-breakpoint positions |
| Resolver | `resolver.rs` | Rule hits, size-tier boundaries (32000 / 32001 / 200000 / 200001), unknown-model fallback |
| Tool truncation | `context/truncate.rs` | Existing `turn/truncation.rs` tests moved verbatim — neither added nor dropped |
| Integration regression | `agent/mod.rs` tests, `turn/tests.rs` | End-to-end behavior unchanged |

### 10.1 Contract suite macro

```rust
strategy_contract_suite!(small_contract,  SmallWindowStrategy::new(16_000));
strategy_contract_suite!(medium_contract, MediumWindowStrategy::new(128_000));
strategy_contract_suite!(large_contract,  LargeWindowStrategy::new(500_000));
strategy_contract_suite!(claude_contract,
    ClaudeStrategy::new(Box::new(MediumWindowStrategy::new(200_000))));
```

Covered checks:

1. `render(empty_conversation)` contains only the System message.
2. `render(single_turn)` contains at least one non-System message.
3. `render(oversized_conversation).stats.sent_tokens <= ctx_window`.
4. `render(oversized_conversation)` still contains the last turn.
5. Outgoing sequence has no orphaned `tool_call` / `tool_result` and
   no empty `AssistantWithToolCalls`.
6. `compression_plan(small_conv)` returns `None` below threshold.
7. `compression_plan(large_conv)` returns `Some(plan)` with
   `messages_to_remove > 0` above threshold.

### 10.2 Regression safety net for Medium

Medium must be byte-for-byte equivalent to the current code:

```rust
#[test]
fn medium_render_matches_current_behavior() {
    let old = Conversation::to_provider_messages_budgeted_legacy(&conv, sys, 128_000);
    let new = MediumWindowStrategy::new(128_000).render(&conv, sys);
    assert_eq!(old.0, new.messages);
    assert_eq!(old.1.sent_tokens, new.stats.sent_tokens);
}
```

Implementation: retain the current methods renamed with `_legacy`
suffix and `#[cfg(test)]`-gated during the refactor. Only delete them
after the snapshot test passes on a representative fixture set
(drawn from a recent `agentarena` or real session log).

### 10.3 Explicitly not doing

- Property-based fuzzing (proptest) — not worth the added dependency
  for this refactor.
- Verifying `Capabilities` fields affect provider output — they don't
  yet (§ 9).
- LLM summarization latency benchmarks — strategy-layer is pure
  functions; LLM call is the agent's concern.

## 11. Migration notes

- `turn/truncation.rs` file is deleted, but the function signatures
  (`truncate_output`, etc.) are re-exported from `context/truncate.rs`.
  Callers in `agent::tool_dispatch` only need an import change.
- `Conversation::to_provider_messages_budgeted`, `needs_compression`,
  `build_compression_content` are removed from `Conversation`. The
  three or four call sites in `agent/mod.rs` switch to
  `self.context.render(...)`, `self.context.should_compress(...)`,
  `self.context.compression_plan(...)`.
- `Conversation::apply_compression` stays put; `agent` still calls it
  after running the LLM summary.
- `cold_summaries: Vec<String>` stays as a field on `Conversation`
  (data), not on the strategy.

## 12. Non-goals

- Summarizer sub-model selection (deferred).
- Emitting `cache_control: ephemeral` in the Claude payload (deferred).
- Adding a config override to force a specific strategy by name
  (not requested).
- Tuning Small and Large thresholds against real local-model sessions
  — table values in § 6 are starting points; follow-up PRs can tune
  after real-world data.
