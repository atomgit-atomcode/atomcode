# Context Manager Refactor — Design Spec

**Date:** 2026-04-20
**Base branch:** `release/v4.19`
**Status:** Draft v2, pending user review (post team-review integration)

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

1. **Thresholds are baked in** (compress at 50%, hard cut at 80%,
   `KEEP_MESSAGES=20`, `keep_recent=5`, cold zone cap=3). Small local
   models (8K–32K) overflow before compression triggers; very large
   windows (≥200K Gemini, Claude 1M) compress far earlier than needed.
2. **No model-specific hooks.** Claude's prompt cache breakpoints,
   thinking block preservation, and per-provider tool-result handling
   have nowhere to live.
3. **Tool output truncation uses the same budget for every provider.**
   Small windows should cut more aggressively.

Primary driver is **window-size adaptivity**. Model-specific capability
hooks (prompt cache, thinking blocks) and per-strategy tool truncation
budgets ride along. Switching the summarizer to a cheaper sub-model is
**out of scope** for this iteration; the design leaves that extension
point open (§ 5, § 12).

## 2. Scope

**In scope** — all of the following moves into a new `context/` module:

- `conversation/mod.rs`: `to_provider_messages_budgeted`, `needs_compression`,
  `build_compression_content`, `microcompact`, `replace_stale_reads`,
  `clean_message_pipeline`.
- `agent/mod.rs`: the dispatch in `maybe_compress_history` that calls
  `needs_compression` / `build_compression_content` / `apply_compression`.
  The LLM invocation and post-compression state injection stay in `agent`.
- `turn/truncation.rs`: everything (`truncate_output`, `UNIVERSAL_MAX_LINES`,
  `hard_char_limit`, `turn_budget`, `post_process_tool_results`).

**Also in scope — delete dead code** before migrating the rest:

- `Conversation::turns_needing_summary`, `build_summary_content`,
  `apply_summary`, `synthesize_turn_outcome` (all `pub`, unused).
- `AgentLoop::maybe_summarize_old_turns` (`#[allow(dead_code)]`).

These represent an earlier summarization path that the current compression
flow replaced. Deleting them first reduces refactor surface.

**Out of scope — explicitly preserved as-is:**

- `Conversation` as a data container (`messages`, `turn_tracker`,
  `cold_summaries`, `apply_compression`, `add_user_message`).
- `config/provider.rs` fields (`context_window`, `max_tokens`).
- LLM call in `maybe_compress_history` and post-compression state
  restoration in `agent`.
- The turn_tracker invariants enforced by `apply_compression`.

## 3. Keying: how strategies map to models

Two-stage resolution inside `context::resolve_strategy(model_id, ctx_window)`:

**Model-ID normalization (before any match):**
```
normalized = model_id.trim().to_lowercase();
if let Some(rest) = normalized.strip_prefix_matching(r"^[a-z0-9_-]+/")
    { normalized = rest; }
```
This handles OpenRouter (`anthropic/claude-3-5-sonnet`), Bedrock
(`bedrock/claude-3-opus`), leading/trailing whitespace, mixed case.

**Stage 1 — rule table (prefix match on normalized id).** Only families
with a demonstrable shape difference get an entry. Initial entries:
- `claude-*` → `ClaudeStrategy` wrapping the size-tier strategy (Medium
  for ≤200K, Large for >200K).

**Stage 2 — size-tier fallback** when no rule hits:
- `ctx_window ≤ 32_000` → `SmallWindowStrategy`
- `32_001..=200_000` → `MediumWindowStrategy`
- `> 200_000` → `LargeWindowStrategy`

OpenAI `gpt-*`, Ollama `llama-*`, custom models go through the size tier
alone — zero config required.

## 4. Architecture

```
crates/atomcode-core/src/
├── context/                      ← new module
│   ├── mod.rs                    ← re-exports + resolve_strategy()
│   ├── strategy.rs               ← ContextStrategy facade trait +
│   │                               three role traits + Capabilities +
│   │                               RenderedContext + CompressionPlan +
│   │                               ConversationView
│   ├── resolver.rs               ← normalization + rule table +
│   │                               size-tier fallback
│   ├── small.rs                  ← SmallWindowStrategy (<32K)
│   ├── medium.rs                 ← MediumWindowStrategy (32K–200K,
│   │                               behavior-equivalent to current code)
│   ├── large.rs                  ← LargeWindowStrategy (>200K)
│   ├── claude.rs                 ← ClaudeStrategy: renderer-only
│   │                               decorator wrapping inner strategy
│   ├── render.rs                 ← shared helpers: microcompact,
│   │                               replace_stale_reads, clean_pipeline,
│   │                               drop_digest, absolute floor
│   ├── truncate.rs               ← migrated from turn/truncation.rs
│   │                               (per-result + per-turn budget)
│   ├── telemetry.rs              ← structured tracing events
│   └── tests/
│       ├── mod.rs                ← strategy_contract_suite! macro
│       ├── stagewise.rs          ← legacy vs new, intermediate snapshots
│       ├── long_session.rs       ← 100-turn SmallWindow stress
│       ├── telemetry.rs          ← structured event assertions
│       └── fixtures/             ← replay corpus (5–10 JSON snapshots)
│
├── conversation/                 ← slimmed
│   ├── mod.rs                    ← data container only: messages,
│   │                               turn_tracker, cold_summaries,
│   │                               add_user_message, apply_compression
│   ├── message.rs                ← unchanged
│   └── turn.rs                   ← unchanged
│
├── agent/mod.rs                  ← holds Box<dyn ContextStrategy>;
│                                   pre-renders messages before calling
│                                   TurnRunner; compression dispatches to
│                                   strategy.compression_plan()
├── turn/
│   ├── runner.rs                 ← signature change: accepts pre-rendered
│   │                               RenderedContext, no longer calls
│   │                               Conversation::to_provider_messages*
│   └── truncation.rs             ← deleted
```

Key relationships:

- `AgentLoop` holds one `Box<dyn ContextStrategy>` decided at construction
  from `(model_id, context_window)`.
- `Conversation` is borrowed through a narrow read-only projection
  (`ConversationView<'a>`, § 5). Strategies cannot see concrete fields
  beyond what the view exposes.
- Shared helpers in `render.rs` are called by concrete strategies as
  composition, not inheritance — Rust has no inheritance.
- `ClaudeStrategy` is a **renderer-only** decorator (§ 5 trait split):
  its `CompactionPolicy` and `ToolOutputTruncator` are the inner
  strategy's, verbatim.
- `TurnRunner` no longer owns the render step — `AgentLoop` renders once
  per turn and passes `RenderedContext` in.

## 5. The trait layer — split into three roles + facade

The review surfaced that `render`, `compress`, and `truncate_tool_output`
have different lifecycles (per turn, end of turn, per tool call) and
different test surfaces. Splitting lets `ClaudeStrategy` decorate only
the piece it cares about (render), avoiding delegation boilerplate.

```rust
// A narrow read-only projection over Conversation. Strategies and
// render helpers see only this.
pub struct ConversationView<'a> {
    pub messages: &'a [Message],
    pub turns: &'a [Turn],
    pub cold_summaries: &'a [String],
}

impl Conversation {
    pub fn view(&self) -> ConversationView<'_> { /* trivial */ }
}

pub trait ContextRenderer: Send + Sync {
    fn render(&self, conv: ConversationView<'_>, system_prompt: &str)
        -> RenderedContext;
}

pub trait CompactionPolicy: Send + Sync {
    fn should_compress(&self, conv: ConversationView<'_>, system_tokens: usize) -> bool;
    fn compression_plan(&self, conv: ConversationView<'_>) -> Option<CompressionPlan>;
}

pub trait ToolOutputTruncator: Send + Sync {
    /// Per-result truncation, invoked for each ToolResult as it lands.
    fn truncate_result(&self, result: &mut ToolResult, tool_name: &str);

    /// Per-turn budget pass after all results for a turn are collected.
    /// Matches current turn/truncation.rs::post_process_tool_results.
    fn enforce_turn_budget(&self, results: &mut [ToolResult]);
}

/// Facade that bundles the three roles into one trait object for
/// AgentLoop to hold. Default methods forward — concrete strategies
/// fill in the three sub-traits and get the facade for free.
pub trait ContextStrategy: ContextRenderer + CompactionPolicy + ToolOutputTruncator {
    fn capabilities(&self) -> &Capabilities;
    fn name(&self) -> &'static str;
}

pub struct RenderedContext {
    pub messages: Vec<Message>,
    pub stats: ContextStats,        // system/sent/dropped tokens, msg_count
    pub cache_plan: CachePlan,      // See below; non-Claude strategies use None
}

/// Cache hint returned by renderer, consumed by provider layer.
/// Shipping only two variants this round; `AutomaticPrefix` (OpenAI)
/// and `ImplicitMinTokens(usize)` (Gemini) land additively with the
/// prompt-cache wiring PR — adding enum variants is non-breaking
/// unlike swapping `Vec<usize>` field shape.
pub enum CachePlan {
    None,
    Breakpoints(Vec<usize>),
}

pub struct CompressionPlan {
    pub content_to_summarize: String,
    pub messages_to_remove: usize,  // passed to Conversation::apply_compression
    pub keep_recent_messages: usize,
    /// Extension point for future sub-model summarizer selection.
    /// Populated but **unused** in this iteration — exists so the
    /// sub-model swap is an additive change, not a trait break.
    pub summarizer_hint: Option<SummarizerHint>,
}

#[derive(Default, Clone)]
pub struct Capabilities {
    pub prompt_cache: bool,
    pub preserve_thinking_blocks: bool,
    pub tool_result_in_cache: bool,
    pub max_output_tokens: Option<usize>,
}

/// Deferred extension point — not populated this iteration.
#[derive(Clone)]
pub struct SummarizerHint {
    pub prefer_fast: bool,
    pub prefer_cheap: bool,
}
```

Design rules:

- **Stateless: no I/O, no network, no file reads.** Internal computation
  state (token counts, scratch buffers) is allowed. Cross-turn positional
  state (e.g., prompt-cache anchor drift tracking) is deferred to the
  prompt-cache wiring PR; if needed it will arrive as a separate
  `StrategyState` struct owned by `AgentLoop`, not on the strategy.
- **No LLM calls from the strategy.** `compression_plan` returns the
  text to summarize; `agent::maybe_compress_history` runs the LLM and
  calls `Conversation::apply_compression`.
- **`apply_compression` stays on `Conversation`.** Data mutation with
  turn_tracker invariants is a data-layer concern.
- **State restoration stays in `agent`.** `current_task`,
  `files_edited_this_turn`, `files_read_this_turn` injection is runtime
  state, unrelated to strategy.

`ClaudeStrategy` composition, using the trait split:

```rust
pub struct ClaudeStrategy {
    inner: Box<dyn ContextStrategy>, // concrete is Medium or Large at resolve time
    caps: Capabilities, // prompt_cache=true, preserve_thinking_blocks=true
}

impl ContextRenderer for ClaudeStrategy {
    fn render(&self, conv: ConversationView<'_>, sys: &str) -> RenderedContext {
        let mut r = self.inner.render(conv, sys);
        r.cache_plan = CachePlan::Breakpoints(compute_cache_breakpoints(&r.messages));
        r
    }
}

// Rust has no method overriding via blanket impls, so CompactionPolicy
// and ToolOutputTruncator forward explicitly. Four small delegate
// methods total, which is acceptable — the whole point of the trait
// split was to keep this delegation count bounded.
impl CompactionPolicy for ClaudeStrategy {
    fn should_compress(&self, conv: ConversationView<'_>, sys_tok: usize) -> bool {
        self.inner.should_compress(conv, sys_tok)
    }
    fn compression_plan(&self, conv: ConversationView<'_>) -> Option<CompressionPlan> {
        self.inner.compression_plan(conv)
    }
}
impl ToolOutputTruncator for ClaudeStrategy {
    fn truncate_result(&self, r: &mut ToolResult, name: &str) {
        self.inner.truncate_result(r, name)
    }
    fn enforce_turn_budget(&self, rs: &mut [ToolResult]) {
        self.inner.enforce_turn_budget(rs)
    }
}

impl ContextStrategy for ClaudeStrategy {
    fn capabilities(&self) -> &Capabilities { &self.caps }
    fn name(&self) -> &'static str { "claude" }
}
```

## 6. Strategy parameters

| Strategy | Window | compress_at | hard_cut_at | keep_recent | cold_max | per-result `hard_char_limit` |
|---|---|---|---|---|---|---|
| `SmallWindowStrategy`  | <32K     | 50% | 70% | 3 turns | 2 entries | `ctx/10`, clamped `[4K, 12K]` |
| `MediumWindowStrategy` | 32K–200K | 50% | 80% | 5 turns | 3 entries | `ctx/8`,  clamped `[8K, 32K]` ← current code |
| `LargeWindowStrategy`  | >200K    | 60% | 85% | 8 turns | 5 entries | `ctx/8`,  clamped `[16K, 64K]` |
| `ClaudeStrategy`       | delegates | — | — | — | — | — |

**Medium is the current behavior unchanged.** This refactor must not
alter the existing Claude/OpenAI paths at 128K; Small and Large are
the new branches.

Per-turn budget (`enforce_turn_budget`) is strategy-specific, derived
from the strategy's window tier. The current formula
`(context_window / 4).min(64_000).max(4_000)` is Medium's exact
behavior and is kept verbatim there. Small tightens: `(ctx/5).min(12_000).max(3_000)`.
Large relaxes: `(ctx/4).min(128_000).max(16_000)`. Each strategy owns
its formula as a constant on the concrete type — no shared hook.

## 7. Data flow

### 7.1 Initialization

```
AgentLoop::new(config)
  └─ provider_cfg = config.providers[config.default_provider]
  └─ self.context = resolve_strategy(&provider_cfg.model,
                                     provider_cfg.context_window)
```

### 7.2 Per-turn render (signature change in TurnRunner)

```
agent.run_turn()
  ├─ rendered = self.context.render(self.conversation.view(), &system_prompt)
  ├─ turn_runner.run_with_filter(rendered, ...)   ← accepts RenderedContext
  │      // cache_plan == None → provider ignores; Breakpoints → Claude emits cache_control
  └─ on tool result:
        self.context.truncate_result(&mut result, tool_name)
        // end of turn, before results fed back to LLM:
        self.context.enforce_turn_budget(&mut all_results_this_turn)
```

`TurnRunner::run_with_filter` current signature computes `msgs` internally
by calling `conv.to_provider_messages_budgeted(...)` (`turn/runner.rs:72`).
New signature takes `rendered: RenderedContext` from the caller. This is
part of the refactor, not a future concern.

### 7.3 Compression (end of turn)

```
agent.after_turn()
  ├─ if self.context.should_compress(self.conversation.view(), sys_tokens):
  │     plan = self.context.compression_plan(self.conversation.view())
  │     summary = call_llm_to_summarize(&plan.content_to_summarize)
  │     if summary.is_empty(): summary = plan.content_to_summarize
  │     self.conversation.apply_compression(plan.messages_to_remove, summary)
  │     emit_telemetry(self.context.name(), &plan, &stats)  // § 10.3
  │     inject_recovery_state(...)  // current_task, files_edited — stays in agent
```

## 8. Error handling and invariants

### 8.1 Unknown `model_id`, missing or degenerate `context_window`

| Case | Handling |
|---|---|
| `model_id` matches no rule | `resolve_by_window(ctx)`, silent |
| `context_window == 0` or field absent | use `ctx.max(8000)`, SmallWindow; `tracing::warn!` once at startup |
| `context_window` > 200K (Gemini, Claude 1M) | LargeWindow (see § 12 — XLarge tier deferred) |
| Provider config entirely missing | keep current `unwrap_or(128000)` fallback → Medium |

### 8.2 LLM summarization failure

Unchanged from current behavior: empty summary → fall back to the
mechanical one-liners from `plan.content_to_summarize`. This logic
stays in `agent::maybe_compress_history`.

### 8.3 Render invariants (every strategy must satisfy)

1. At least one non-System message when `conversation.messages` is
   non-empty (absolute floor).
2. The last turn is never dropped (HARD FLOOR).
3. Outgoing message sequence is provider-legal — `tool_call` and
   `tool_result` paired, no empty `AssistantWithToolCalls`.
4. `stats.sent_tokens <= token_budget` after any necessary hard cut.

Enforced via a shared contract test suite (§ 10.1).

### 8.4 `apply_compression` invariants

The existing `CRITICAL INVARIANT` block in
`Conversation::apply_compression` (six assertions around
`start_idx < new_len`, `end_idx <= new_len`, `msg_count > 0`) is
preserved verbatim. Data-layer concern.

## 9. Rollback via `LegacyStrategy` trait object

The refactor touches 9 call sites across `agent/mod.rs`,
`turn/runner.rs`, and `agent/tool_dispatch.rs`. A `#[cfg(test)]`-only
`_legacy` snapshot is insufficient rollback. Branching every call site
on `if config.use_context_strategy { new } else { legacy }` is also
wrong — it spreads the flag to 9 places and creates a real drift bug
if the flag is toggled mid-session between paired operations (e.g.,
`build_compression_content` and `apply_compression` at
`agent/mod.rs:648`).

Instead: make "legacy" itself a `ContextStrategy` implementation.

```rust
/// Thin adapter: wraps the still-intact legacy methods on Conversation
/// and turn/truncation.rs and exposes them through the ContextStrategy
/// facade. Exists for exactly one release as the rollback gate.
pub struct LegacyStrategy { ctx_window: usize, caps: Capabilities }

impl ContextRenderer for LegacyStrategy {
    fn render(&self, conv: ConversationView<'_>, sys: &str) -> RenderedContext {
        // Delegates to Conversation::to_provider_messages_budgeted_impl
        // (renamed, kept `pub(crate)` for this release).
    }
}
// CompactionPolicy / ToolOutputTruncator: same pattern — delegate to
// the legacy Conversation / turn::truncation functions.
impl ContextStrategy for LegacyStrategy { /* caps default, name "legacy" */ }
```

Then `resolve_strategy` is the single gate:

```rust
pub fn resolve_strategy(
    model_id: &str,
    ctx_window: usize,
    use_new: bool,  // read ONCE at AgentLoop construction, never re-read
) -> Box<dyn ContextStrategy> {
    if !use_new { return Box::new(LegacyStrategy::new(ctx_window)); }
    // ... normal resolution ...
}
```

Config flag:

```rust
pub struct Config {
    /// Enable the new strategy layer. Read once when AgentLoop is
    /// constructed; runtime changes to this flag are ignored for the
    /// duration of the session. Default true after this refactor.
    /// Remove this flag, LegacyStrategy, and the preserved legacy
    /// functions one release later.
    #[serde(default = "default_true")]
    pub use_context_strategy: bool,
}
```

Consequences:

- All 9 call sites become flag-blind — they always call
  `self.context.xxx()`. No `if` branches outside `resolve_strategy`.
- No mid-session toggle drift: `AgentLoop` stores the resolved
  `Box<dyn ContextStrategy>` at construction and never re-reads config.
- Legacy methods on `Conversation` and `turn/truncation.rs` stay `pub(crate)`
  and functional (NOT `#[cfg(test)]`), feeding `LegacyStrategy`.
- Cleanup PR one release later = delete `LegacyStrategy`, delete flag,
  delete preserved legacy functions. No call-site changes needed.
- Flag default flip from `false` → `true` lives in **its own commit**,
  gated on: stagewise tests green + fixture replay green + **telemetry
  parity observed in production** (not just CI green).

## 10. Capabilities — populated but not read this iteration

`Capabilities` fields are filled by each strategy (Claude sets
`prompt_cache=true` and `preserve_thinking_blocks=true`; others leave
defaults) but the provider layer does **not** read them in this
refactor. Actual prompt-cache wiring (emitting `cache_control: ephemeral`
in the Claude payload) is a follow-up PR. This keeps the refactor
surface minimal.

## 11. Testing strategy

| Layer | File(s) | What |
|---|---|---|
| Shared invariants | `context/tests/mod.rs` — `strategy_contract_suite!` macro | The 4 invariants in § 8.3, once per strategy |
| Strategy-specific | `small.rs`, `medium.rs`, `large.rs`, `claude.rs` `#[cfg(test)]` | Threshold-specific behavior: Small's 50% compress, Large's `keep_recent=8`. Claude decorator test asserts `cache_plan == Breakpoints(_)` with non-empty vector on a known-large render — no golden positions (consumer deferred) |
| Resolver | `resolver.rs` | Normalization, rule hits, size-tier boundaries, unknown-model fallback, edge cases (empty, mixed case, `provider/` prefix), `resolver_conflict_claude_small` locks `claude-3-haiku + ctx=8000 → ClaudeStrategy(SmallWindow)` |
| Tool truncation | `context/truncate.rs` | Existing `turn/truncation.rs` tests moved verbatim + `enforce_turn_budget_per_strategy` table test (Small `ctx/5`, Medium `ctx/4`, Large `ctx/4` with differing clamp bands, identical input) |
| Stagewise equivalence | `context/tests/stagewise.rs` | Legacy vs new compared after **each** in-place mutation (microcompact, replace_stale_reads, clean_pipeline), not just final output; includes `after_clean_pipeline == final_output` self-consistency assertion on both sides |
| Fixture replay | `context/tests/fixtures/*.json` | 5–10 serialized `Conversation` snapshots replayed through legacy + new, asserted byte-equal under Medium |
| Legacy path regression | `context/tests/legacy_path.rs` | End-to-end agent test run under `use_context_strategy=false` — catches LegacyStrategy rot once default flips |
| Integration regression | `agent/mod.rs` tests | End-to-end behavior unchanged |
| Long-session stress | `context/tests/long_session.rs` | 100-turn synthetic loop under SmallWindow(8K) — no panic, bounded growth, invariants per turn |
| Telemetry emission | `context/tests/telemetry.rs` | Structured tracing event fires on each compression with `strategy/ctx/removed/kept` |

### 11.1 Contract suite macro

```rust
strategy_contract_suite!(small_contract,  SmallWindowStrategy::new(16_000));
strategy_contract_suite!(medium_contract, MediumWindowStrategy::new(128_000));
strategy_contract_suite!(large_contract,  LargeWindowStrategy::new(500_000));
strategy_contract_suite!(claude_contract,
    ClaudeStrategy::new(MediumWindowStrategy::new(200_000)));
```

Each macro invocation expands to:

1. `render(empty)` contains only the System message.
2. `render(single_turn)` contains ≥1 non-System message.
3. `render(oversized).stats.sent_tokens <= ctx_window`.
4. `render(oversized)` still contains the last turn.
5. Outgoing sequence has no orphan `tool_call` / `tool_result` and no
   empty `AssistantWithToolCalls`.
6. `compression_plan(small_conv) == None`.
7. `compression_plan(large_conv).unwrap().messages_to_remove > 0`.

### 11.2 Stagewise equivalence (Medium only)

The bare "final output byte-equal" assertion is insufficient — two
pipelines with swapped or skipped intermediate mutations can converge
on a given fixture yet diverge on others. Medium equivalence must
assert equality **after each mutation**:

```rust
#[test]
fn medium_stagewise_matches_legacy() {
    for fixture in load_fixtures() {
        let (legacy_stages, legacy_final) =
            Conversation::render_with_stage_snapshots_legacy(&fixture, SYS, 128_000);
        let (new_stages, new_final) =
            MediumWindowStrategy::new(128_000)
                .render_with_stage_snapshots(fixture.view(), SYS);
        assert_eq!(legacy_stages.after_microcompact, new_stages.after_microcompact);
        assert_eq!(legacy_stages.after_replace_stale, new_stages.after_replace_stale);
        assert_eq!(legacy_stages.after_clean_pipeline, new_stages.after_clean_pipeline);
        assert_eq!(legacy_final, new_final);
    }
}
```

`render_with_stage_snapshots` is an **inherent** `#[cfg(test)]` method
on each concrete strategy type (not on the `ContextStrategy` trait) and
on `Conversation` (for the legacy side). Both sides call into the same
`render.rs` helper functions the production path calls — the snapshots
are captured inline, not by a parallel reimplementation. The
`after_clean_pipeline == final_output` self-consistency assertion runs
on both sides, catching instrumentation drift if one side snapshots
before cleanup and the other after.

### 11.3 Fixture corpus

5–10 serialized `Conversation` snapshots under
`crates/atomcode-core/src/context/tests/fixtures/`, each JSON file
with a schema version header AND a `source: "replay" | "synthetic"`
tag.

**CI guard (mandatory):**
```rust
#[test]
fn fixture_corpus_has_real_replay() {
    let fixtures = load_fixtures();
    assert!(fixtures.len() >= 5, "fixture corpus below minimum");
    assert!(
        fixtures.iter().any(|f| f.source == "replay"),
        "at least one replayed fixture required — synthetic-only corpus is self-consistency theater"
    );
}
```

Sourcing the first replay (one-time work at implementation start):
grep the repo for any existing agentarena / swebench / datalog
artifact that serializes a `Conversation`. If none exists, capture one
by running `atomcode` against a real repo for a 10+ turn session with
datalog enabled and extracting the `Conversation` state at session
end.

Remaining 4+ fixtures can be synthetic, covering shapes: short
(<5 msgs), at compression threshold (~50% of 128K), tool-heavy
(20+ `tool_result` messages), thinking-block-heavy, stale-read-heavy,
oversized (>80% of 128K forcing hard cut).

### 11.4 Resolver edge-case tests

- `ctx=0`, `ctx=1`, `ctx=u32::MAX as usize`.
- `model_id=""`, `model_id=" claude-3 "`, `model_id="CLAUDE-3"`.
- `model_id="anthropic/claude-3-5-sonnet"` (OpenRouter),
  `model_id="bedrock/claude-3-opus"`.
- Rule + tier conflict: `claude-3-haiku` with `ctx=8000` (small) — does
  Claude wrapper still wrap? (Expected: yes, it always wraps.)

### 11.5 Explicitly not doing

- Property-based fuzzing (proptest) — not worth the dependency this round.
- Asserting `Capabilities` affect provider output — they don't yet (§ 10).
- LLM summarization latency benchmarks.
- Stable-prefix invariant tests — deferred with the prompt-cache wiring.

## 12. Post-merge observability

One structured tracing event per compression in `context/telemetry.rs`:

```rust
tracing::info!(
    target: "context.compress",
    strategy = ctx.name(),
    context_window = ctx_window,
    messages_before = before,
    messages_removed = removed,
    messages_kept = kept,
    sys_tokens = sys_tokens,
    sent_tokens_before = sent_before,
    sent_tokens_after = sent_after,
    "context compressed"
);
```

One event on each `render` that triggers a hard cut, so we can
distinguish "SmallWindow is firing on Ollama" from "nobody ran Ollama":

```rust
tracing::warn!(
    target: "context.hard_cut",
    strategy = ctx.name(),
    dropped_tokens = dropped,
    "context hard cut"
);
```

No new telemetry crate dependency — uses the existing `tracing`
infrastructure already present.

## 13. Migration notes

### 13.1 Enumerated call sites (updated per review)

All call sites of the methods being extracted:

| File | Line(s) | Method | Change |
|---|---|---|---|
| `turn/runner.rs` | 72 | `to_provider_messages_budgeted` | Signature change: `run_with_filter` accepts pre-rendered `RenderedContext` from caller |
| `agent/mod.rs` | 648 | `build_compression_content` + `apply_compression` | Task-boundary cleanup path — route through `self.context.compression_plan` |
| `agent/mod.rs` | 791 | `to_provider_messages_budgeted` | Replace with `self.context.render(conv.view(), sys)` |
| `agent/mod.rs` | 813 | `to_provider_messages_budgeted` | Warm-cache estimate path — replace with `self.context.render(...)` |
| `agent/mod.rs` | 1357 | `to_provider_messages_budgeted` | Replace with `self.context.render(...)` |
| `agent/mod.rs` | 1442 | `needs_compression` | Replace with `self.context.should_compress(conv.view(), sys_tokens)` |
| `agent/mod.rs` | 1446 | `build_compression_content` | Replace with `self.context.compression_plan(conv.view())` |
| `agent/mod.rs` | 1492 | `apply_compression` | Unchanged (stays on `Conversation`) |
| `agent/tool_dispatch.rs` | 136 | `truncate_output` | Import changes to `context::truncate_result` via `self.context` |

If call-site discovery during implementation surfaces additional call
sites (the review found them by grep; human audit during implementation
may find more), append to this table — do not silently widen scope.

### 13.2 Visibility widening

The following are currently private `fn` in `conversation/mod.rs` and
must become `pub(crate)` to move to `context/render.rs`:

- `microcompact` (line ~1007)
- `replace_stale_reads` (line ~1180)
- `clean_message_pipeline` (line ~901)

Do the visibility bump in the first commit of the refactor, move them
in a later commit.

### 13.3 `turn/truncation.rs` deletion

File deleted. Function names (`truncate_output`) re-exported from
`context/truncate.rs` so `agent::tool_dispatch` only changes imports.
`post_process_tool_results` moves onto `ToolOutputTruncator` as
`enforce_turn_budget`.

### 13.4 Dead-code removal (first commit)

Delete before anything else:

- `Conversation::turns_needing_summary` (line 578)
- `Conversation::build_summary_content` (line 616)
- `Conversation::apply_summary` (line 680)
- `Conversation::synthesize_turn_outcome` (line 807)
- `AgentLoop::maybe_summarize_old_turns` (line 1519,
  `#[allow(dead_code)]`)
- Any associated helper functions used only by these.

Confirm with a single grep pass that no production caller remains.

### 13.5 Commit sequence (suggested, not mandatory)

1. Delete dead summary surface (§ 13.4). One commit.
2. Create `context/` skeleton: role traits (`ContextRenderer`,
   `CompactionPolicy`, `ToolOutputTruncator`), facade trait
   `ContextStrategy`, `Capabilities`, `RenderedContext`, `CachePlan`,
   `CompressionPlan`, `ConversationView<'a>`. Empty Small/Medium/Large
   impls. No call-site changes.
3. Widen visibility of `microcompact` / `replace_stale_reads` /
   `clean_message_pipeline` to `pub(crate)`. Add `Conversation::view()`
   implementation (uses `ConversationView` from step 2).
4. Move `turn/truncation.rs` → `context/truncate.rs`. Move tests with it.
5. Implement `MediumWindowStrategy::render` + `CompactionPolicy` +
   `ToolOutputTruncator` as thin wrappers over the migrated helpers.
6. Implement `LegacyStrategy` wrapping preserved legacy functions on
   `Conversation` and `turn/truncation.rs` (renamed helpers). Add
   `use_context_strategy` config flag (default `false` initially).
   Change `resolve_strategy` to return `LegacyStrategy` when flag is
   false, read flag once at `AgentLoop::new`. Rewrite all 9 call sites
   to call `self.context.xxx()` (flag-blind).
7. Add stagewise equivalence + fixture replay tests for Medium,
   including `fixture_corpus_has_real_replay` CI guard. All other
   Medium tests (contract suite, resolver edge cases, enforce_turn_budget).
8. **Flip flag default `false` → `true` in its own commit.** Gate:
   stagewise + fixture replay green on CI, **telemetry parity
   confirmed** (compression counts / dropped tokens between legacy
   and new within agreed tolerance on a canary session). Do not
   bundle this flip with any code change.
9. Fill in `SmallWindowStrategy` and `LargeWindowStrategy` thresholds
   + differentiated truncation budgets + per-strategy tests.
10. Add `ClaudeStrategy` (renderer-only decorator, `cache_plan =
    Breakpoints(...)`) and wire into resolver rule table. Add
    `resolver_conflict_claude_small` test.
11. Add telemetry emissions at compression and hard-cut sites.
12. Add long-session stress test, legacy_path regression test,
    telemetry emission test.

## 14. Non-goals (deferred to follow-up PRs)

Deliberately not tackled this round — called out so reviewers can see
the scope line:

- **1M-token tier (`XLargeWindowStrategy`)** for Gemini 2 and Claude
  1M Sonnet. Today they fall into `LargeWindowStrategy` — acceptable
  interim.
- **`ReasoningStrategy` decorator** for o1/o3/claude-thinking (cold-zone
  thinking block stripping, encrypted reasoning item handling).
  `preserve_thinking_blocks` capability flag is populated but has no
  consumer.
- **Additional `CachePlan` variants.** `None` and `Breakpoints(Vec<usize>)`
  ship in this refactor on `RenderedContext.cache_plan`. `AutomaticPrefix`
  (OpenAI auto-cache) and `ImplicitMinTokens(usize)` (Gemini implicit)
  are additive enum variants landing with the prompt-cache wiring PR.
  The provider layer does not read `cache_plan` yet this round.
- **Stable-prefix invariant** (rendering twice produces byte-identical
  prefix) — required for OpenAI / Gemini cache hits, lands with
  `CachePlan`.
- **Summarizer sub-model selection.** `CompressionPlan.summarizer_hint`
  is populated unused; the agent layer will consume it in a later PR.
- **Per-model-version rules** (Haiku vs Opus threshold tuning).
- **`tool_result_in_cache` affecting truncation budget** (Claude cached
  tool outputs can be larger).
- **Explicit `strategy = "..."` config override.** Users rely on
  auto-resolution for now.

## 15. Open questions

None. Base branch is `release/v4.19` (user decision, 2026-04-20).
All review items are either folded in or explicitly deferred to § 14.
