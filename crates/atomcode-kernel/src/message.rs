use crate::stream::TokenUsage;
use crate::tool::ToolCall;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Reserved synthetic-message origin used when a runtime without a dedicated
/// cold-summary lane persists a legacy cold-summary snapshot inline as a
/// kernel [`Message`]. The daemon's legacy importer and the TUI's
/// `cold_summaries_from_messages` both match on this exact string — it is a
/// stable disk/message encoding contract and **must not change**.
pub const LEGACY_COLD_SUMMARY_ORIGIN: &str = "atomcode.legacy_cold_summary";

/// Stable payload prefix paired with [`LEGACY_COLD_SUMMARY_ORIGIN`].
/// Written as the first bytes of a cold-summary synthetic message's `text`
/// so consumers can strip it and recover the bare summary. **Must not change.**
pub const LEGACY_COLD_SUMMARY_PREFIX: &str =
    "[Earlier conversation history — compressed OLDER context, not a user instruction]\n";

/// Extract the bare cold-summary strings from a kernel message list: the
/// synthetic messages tagged [`LEGACY_COLD_SUMMARY_ORIGIN`], with their
/// [`LEGACY_COLD_SUMMARY_PREFIX`] stripped. Mirrors the daemon importer's decode
/// (`snapshot_to_core`). Lives here (next to the constants) so every consumer —
/// the TUI, the daemon transport — shares one definition instead of open-coding
/// the match. Order-preserving.
pub fn cold_summaries_from_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN))
        .filter_map(|m| m.text.strip_prefix(LEGACY_COLD_SUMMARY_PREFIX))
        .map(|summary| summary.to_string())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A neutral inline image attached to a [`Message`] (user input). `data` is base64-encoded
/// bytes; `media_type` is the MIME type (e.g. `"image/png"`). The kernel only STORES and
/// FORWARDS it — each provider ADAPTER decides the wire shape (OpenAI `image_url` data URL
/// vs Anthropic base64 `source`). Turning an image into text for a non-vision model (VL
/// preprocessing) is an L1/L2 concern, NEVER the kernel's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub media_type: String,
    pub data: String,
}

/// One unit of an assistant message's REASONING/THINKING, preserved losslessly so a
/// thinking-block provider adapter (L1) can echo it back VERBATIM next turn.
///
/// The flat [`Message::reasoning`] string is sufficient for the OpenAI-compatible
/// `reasoning_content` path (plain text, no signature). This richer per-unit shape
/// exists for providers whose thinking carries an OPAQUE round-trip token that must be
/// replayed exactly — Anthropic extended thinking (`signature`), OpenAI Responses
/// (`encrypted_content`), Gemini (`thoughtSignature`). The kernel only STORES the
/// mechanism (text + opaque + attribution); the ECHO policy stays in L1.
///
/// INVARIANT: `opaque.is_some()` ⇒ `provider.is_some()`. An opaque token is
/// PROVIDER-BOUND — replaying it to a different provider (or after a model swap) fails
/// hard — so an adapter uses `provider` to echo a token back ONLY to its own backend
/// and to leave another vendor's block untouched. A REDACTED thinking block carries an
/// empty `text` with `opaque = Some(data)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    /// Human-readable thinking (may be empty/redacted).
    pub text: String,
    /// OPAQUE round-trip payload, stored and echoed VERBATIM (Anthropic `signature` /
    /// OpenAI `encrypted_content` / Gemini `thoughtSignature`). The kernel NEVER parses
    /// or re-serializes it (any re-encode invalidates the signature). `None` for a
    /// plain-text reasoning unit with no signature.
    #[serde(default)]
    pub opaque: Option<String>,
    /// Attribution — which provider produced `opaque`. INVARIANT: `opaque.is_some()` ⇒
    /// `provider.is_some()`.
    #[serde(default)]
    pub provider: Option<String>,
}

/// Kernel-native per-message execution stats, recorded at on_model_response.
/// A SIDECAR — never part of `text` — so storing it never changes the bytes the
/// LLM sees (prefix-cache safety). The renderer (pre_request) chooses whether to
/// PROJECT a summary of it into the request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub tokens: TokenUsage,
    pub elapsed_ms: u64,
    /// Thinking/reasoning phase duration in milliseconds.
    #[serde(default)]
    pub reasoning_elapsed_ms: u64,
    pub ctx_window: u32,
    pub used_tokens: u32,
    pub utilization: f32,
    pub round: u32,
    /// Correlation IDs (observability): `turn_id` = which user turn produced this
    /// message; `request_id` = which LLM request (session-global, monotonic). ADDITIVE:
    /// `#[serde(default)]` so an older snapshot (no these fields) still deserializes (→ 0).
    #[serde(default)]
    pub turn_id: u64,
    #[serde(default)]
    pub request_id: u64,
    /// The PROVIDER's own response id (opaque upstream handle), for cross-referencing
    /// the provider's server-side logs. `None` if the provider/adapter surfaced none.
    /// ADDITIVE: `#[serde(default)]`.
    #[serde(default)]
    pub provider_response_id: Option<String>,
    /// The model identity reported by the provider for this response. This is kept
    /// separately from the requested runtime model so routing mismatches are observable.
    /// ADDITIVE: older snapshots deserialize it as `None`.
    #[serde(default)]
    pub provider_model: Option<String>,
    /// Injected session identity (mirrors `TurnCtx.session_id`) so a STORED message
    /// carries the FULL correlation set (session → turn → round/request) on its own.
    /// ADDITIVE.
    #[serde(default)]
    pub session_id: Option<String>,
    /// How the model ENDED this response — the response's "code": `"stop"` (text done),
    /// `"tool_calls"` (wants tools), or `"length"` (truncated). Derived by the kernel
    /// from the observed stream (tool calls present / truncated flag). ADDITIVE (empty
    /// string for older snapshots).
    #[serde(default)]
    pub finish_reason: String,
}

/// Provider-neutral message.
///
/// Derives `Serialize, Deserialize` so a conversation is LOSSLESSLY persistable
/// and resumable: every field — `role`, `text`, `tool_calls`, `tool_call_id`,
/// `is_error`, `meta` — survives a serde round-trip. (Contrast the retired, lossy
/// `MessageSnapshot`, which dropped `tool_calls`/`tool_call_id` and stringified
/// `Role` via `Debug`.) `PartialEq` lets round-trip equality be asserted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// True iff this is a tool RESULT that failed — carried to the provider as the
    /// tool_result `is_error` flag so a real adapter can tell the model the call
    /// errored. Always false for non-result messages.
    pub is_error: bool,
    /// Kernel-native execution stats (sidecar). Never implicitly rendered into
    /// `text` — projecting to the LLM is the renderer's explicit choice.
    pub meta: Option<MessageMeta>,
    /// True iff this message was INJECTED BY THE KERNEL (a cold-compaction summary
    /// or a resume note) rather than produced by the real model/user. ADDITIVE:
    /// `#[serde(default)]` so a v1 snapshot (no `synthetic` field) still
    /// deserializes (→ false). `sacred_floor` reads this to find the FIRST REAL
    /// (non-synthetic) user message — so a synthetic resume/summary message that
    /// precedes the real prompt is never mistaken for the sacred task anchor.
    #[serde(default)]
    pub synthetic: bool,
    /// Internal provenance for assistant messages produced by kernel-driven control rounds.
    /// Empty for normal user/model messages. Example: "verify_cadence".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_origin: Option<String>,
    /// The model's REASONING/THINKING output for an ASSISTANT message — `None` for
    /// non-assistant messages and for assistant responses from non-thinking models.
    /// A purely STORED field: thinking models (Anthropic extended thinking,
    /// DeepSeek) require the PRIOR turn's reasoning to be echoed back alongside the
    /// tool calls or the request is rejected / the prompt cache breaks. The kernel
    /// only STORES it losslessly here (so it survives serde, resume, and compaction
    /// of surviving messages); a provider adapter (L1) decides the wire echo-back
    /// format — OUT OF SCOPE here. ADDITIVE: `#[serde(default)]` so a v1 snapshot
    /// (no `reasoning` field) still deserializes (→ None).
    ///
    /// FUTURE: when an L1 adapter for thinking-block providers (Anthropic extended
    /// thinking / OpenAI Responses / Gemini) actually PRODUCES opaque tokens, upgrade
    /// this representation to carry, per reasoning unit:
    ///
    /// ```text
    /// text:     Option<String>   human-readable thinking (may be empty/redacted);
    ///                            today's flat `reasoning` IS this.
    /// opaque:   Option<String>   OPAQUE round-trip payload, stored & echoed VERBATIM:
    ///                            Anthropic `signature` / OpenAI `encrypted_content`
    ///                            (or `rs_` id) / Gemini `thoughtSignature`. The kernel
    ///                            NEVER parses or re-serializes it (any re-encode
    ///                            invalidates the signature).
    /// provider: Option<String>   attribution (which provider produced `opaque`).
    ///                            INVARIANT: opaque.is_some() => provider.is_some().
    ///                            REQUIRED: an opaque token is PROVIDER-BOUND; replaying
    ///                            it to a different provider / after a model swap fails
    ///                            hard (OpenAI invalid_encrypted_content, Gemini
    ///                            missing/invalid signature). An L1 adapter uses it to
    ///                            echo a signature back ONLY to the same provider, and
    ///                            to avoid stripping another vendor's block.
    /// ```
    ///
    /// For interleaved multi-block thinking (one turn -> several signed blocks), lift
    /// the unit to a `Vec<ReasoningBlock>` to preserve order + per-block signatures.
    /// Add it the same additive way (`#[serde(default)]`) so old snapshots still load.
    /// The ECHO policy (when/whether/to-whom) stays in L1; the kernel only stores the
    /// mechanism (lossless text + opaque + attribution). The CURRENT GLM/DeepSeek
    /// OpenAI-compatible path needs NONE of this — its reasoning is plain text with no
    /// signature, fully served by the flat `reasoning` below.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Inline images attached to this message (multimodal user input). ADDITIVE:
    /// `#[serde(default)]` so an older snapshot (no `images`) still deserializes (→
    /// empty). Empty for every non-image message — so a text-only path keeps rendering
    /// `content` as a STRING unchanged (prefix-cache safety); only a NON-empty `images`
    /// makes an adapter switch to the array `content` shape. See [`ImageContent`].
    #[serde(default)]
    pub images: Vec<ImageContent>,
    /// SIGNED/OPAQUE reasoning units for an ASSISTANT message, in stream order — the
    /// rich twin of the flat [`Message::reasoning`]. Empty for every message the
    /// OpenAI-compatible path produces (its reasoning is plain text in `reasoning`);
    /// an Anthropic-style adapter populates it (one entry per thinking / redacted
    /// block) so it can replay the signed blocks VERBATIM on the next request. The
    /// kernel only STORES them; the echo policy is the L1 adapter's. ADDITIVE:
    /// `#[serde(default)]` so a snapshot without this field still deserializes (→
    /// empty). See [`ReasoningBlock`].
    #[serde(default)]
    pub reasoning_blocks: Vec<ReasoningBlock>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_calls,
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }
    /// A tool RESULT. `is_error` is now STORED (a real adapter must echo it to the
    /// provider) — it was previously dropped, losing tool failure state.
    pub fn tool_result(
        call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            text: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
            is_error,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }
    /// A KERNEL-INJECTED synthetic `Role::User` message (compaction cold summary or
    /// resume note). It carries `synthetic = true` so `sacred_floor` skips it when
    /// locating the first REAL user prompt. The `Role::User` choice (not System)
    /// is deliberate: a System-role injection would risk being folded into the
    /// frozen system prefix by a downstream consecutive-system merger and rewrite
    /// the whole cached prefix — see `ctx/render.rs` in production. Inserted
    /// after the system message, it preserves the frozen system prefix.
    pub fn synthetic_user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: true,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }
    /// A user message carrying inline `images` (multimodal input). Identical to
    /// [`Message::user`] when `images` is empty.
    pub fn user_with_images(text: impl Into<String>, images: Vec<ImageContent>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images,
            reasoning_blocks: vec![],
        }
    }
    /// A KERNEL-INJECTED `Role::User` message carrying `images` — used by the agent
    /// loop to surface images a TOOL produced (e.g. `read_file` on a picture) to the
    /// model, since a provider only serializes images on a user message, never a tool
    /// message. `synthetic = true` so `sacred_floor` skips it when locating the real
    /// task prompt (same rationale as [`Message::synthetic_user`]).
    pub fn synthetic_user_with_images(text: impl Into<String>, images: Vec<ImageContent>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: true,
            internal_origin: None,
            reasoning: None,
            images,
            reasoning_blocks: vec![],
        }
    }

    /// Approximate token count for this message — a byte heuristic (~4 bytes/token;
    /// images ≈ 1600 tokens each). Used ONLY as a FALLBACK for context-pressure when
    /// the provider omits a usage report (e.g. a gateway that returns an empty 200, or
    /// drops the usage chunk emitted after `finish_reason`). The EXACT prompt total
    /// always comes from the provider's usage when present; this keeps utilization —
    /// and thus auto-compaction — tracking when it is absent (without it, a non-
    /// reporting provider records utilization 0.0 forever and never compacts).
    /// Mirrors the legacy estimate heuristic (see `atomcode-coding`'s telemetry copy).
    pub fn estimate_tokens(&self) -> u32 {
        // Images dominate when present (vision ≈ 1600 tok each).
        if !self.images.is_empty() {
            return ((self.text.len() / 4).max(1) + self.images.len() * 1600 + 4) as u32;
        }
        let byte_count = if self.role == Role::Tool {
            self.text.len() + 10 // tool result + small wrapper overhead
        } else if !self.tool_calls.is_empty() {
            let calls: usize = self
                .tool_calls
                .iter()
                .map(|tc| tc.name.len() + tc.arguments.len() + 20)
                .sum();
            self.text.len() + calls + self.reasoning.as_ref().map_or(0, |r| r.len())
        } else {
            self.text.len()
        };
        ((byte_count / 4).max(1) + 4) as u32
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    /// The PREFIX-GENERATION marker (a SIDECAR, NEVER serialized into message text
    /// and NEVER sent to the LLM). It records "the stored prefix bytes changed
    /// here — a new cache epoch began", i.e. the one point where the append-only
    /// prefix relation is allowed to break. Today ONLY a COMMITTED compaction bumps
    /// it (see `apply_plan`); when future system/tool mutation seams land, those
    /// would bump it too (out of scope now). A new `Conversation` starts at epoch
    /// 0. ADDITIVE: `#[serde(default)]` so a v1 snapshot still deserializes (→ 0).
    #[serde(default)]
    pub cache_epoch: u64,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }

    /// For any assistant message whose `tool_calls` lack a matching tool-result
    /// (identified by `tool_call_id`), APPEND a synthetic `(cancelled)` tool
    /// result. This keeps the API valid (every tool_use paired with a
    /// tool_result) after a cancel mid-turn.
    ///
    /// Carried faithfully from production
    /// (`conversation::Conversation::backfill_cancelled_tool_results`). It is
    /// APPEND-ONLY — existing messages are never mutated or reordered — so it
    /// preserves the prefix-cache invariant guarded by `tests/cache_prefix.rs`.
    pub fn backfill_cancelled_tool_results(&mut self) {
        // Collect call_ids that already have results.
        let mut seen_result_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for m in &self.messages {
            if let Some(id) = &m.tool_call_id {
                seen_result_ids.insert(id.clone());
            }
        }

        // Find assistant tool_calls with no matching result.
        let mut missing: Vec<String> = Vec::new();
        for m in &self.messages {
            if m.role == Role::Assistant {
                for tc in &m.tool_calls {
                    if !seen_result_ids.contains(&tc.id) {
                        missing.push(tc.id.clone());
                    }
                }
            }
        }

        // Append one (cancelled) result per dangling call (append-only).
        for id in missing {
            self.messages
                .push(Message::tool_result(id, "(cancelled)", true));
        }
    }

    /// Make a message vec API-VALID in place: every assistant `tool_call` is
    /// paired with EXACTLY ONE following `tool_result`, and no `tool_result` is an
    /// ORPHAN (a `Role::Tool` message whose `tool_call_id` matches no assistant
    /// `tool_call`). The kernel — not a strategy — owns this invariant, so a buggy
    /// strategy (or an externally-supplied/legacy snapshot) can never hand the
    /// provider an illegal "messages" payload.
    ///
    /// This is a strict SUPERSET of `backfill_cancelled_tool_results`: it both
    /// (a) DROPS orphan results AND (b) backfills danglings — and, crucially, it
    /// inserts each missing result IMMEDIATELY AFTER its assistant message
    /// (preserving the result-follows-call ordering), not appended at the end. (The
    /// append-only cancel path keeps using `backfill_cancelled_tool_results`, whose
    /// danglings are trailing so an end-append is fine there and existing tests stay
    /// green.)
    ///
    /// Two passes, ordering-preserving:
    ///   (a) ORPHAN SCRUB — collect the set of LIVE `tool_calls[].id` across all
    ///       messages (as OWNED `String`s, to avoid a borrow conflict with the
    ///       following rebuild), then `retain` only the `Role::Tool` messages whose
    ///       `tool_call_id` is in that set.
    ///   (b) DANGLING REPAIR — rebuild the Vec: push each message, then for any of
    ///       its assistant `tool_calls` whose id has NO matching surviving result,
    ///       push a synthetic `(cancelled)` result right after it.
    pub fn repair_pairing(msgs: &mut Vec<Message>) {
        // (a) ORPHAN SCRUB. Collect OWNED live call ids first to release the borrow
        // before the mutating `retain`.
        let live_call_ids: std::collections::HashSet<String> = msgs
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|tc| tc.id.clone()))
            .collect();
        msgs.retain(|m| match (&m.role, &m.tool_call_id) {
            // A tool RESULT survives only if its id matches a live tool_call.
            (Role::Tool, Some(id)) => live_call_ids.contains(id),
            // Non-result messages (and the degenerate Tool-without-id) pass through.
            _ => true,
        });

        // The set of result ids that NOW survive (after the scrub) — these are the
        // calls that are already paired.
        let result_ids: std::collections::HashSet<String> =
            msgs.iter().filter_map(|m| m.tool_call_id.clone()).collect();

        // (b) DANGLING REPAIR. Rebuild, inserting each missing result right after
        // its assistant message so the result FOLLOWS its call.
        let mut rebuilt: Vec<Message> = Vec::with_capacity(msgs.len());
        for m in msgs.drain(..) {
            let stubs: Vec<Message> = if m.role == Role::Assistant {
                m.tool_calls
                    .iter()
                    .filter(|tc| !result_ids.contains(&tc.id))
                    .map(|tc| Message::tool_result(tc.id.clone(), "(cancelled)", true))
                    .collect()
            } else {
                Vec::new()
            };
            rebuilt.push(m);
            rebuilt.extend(stubs);
        }
        *msgs = rebuilt;
    }

    /// The number of LEADING messages that must NEVER be removed by compaction:
    /// a leading `Role::System` message (if present) PLUS up to and INCLUDING the
    /// FIRST NON-SYNTHETIC (`synthetic == false`) `Role::User` message. This keeps
    /// the persona + the original task prompt alive across every compaction so a
    /// resumed timeline still opens on the human's ask.
    ///
    /// Returns an INDEX `floor` such that `messages[..floor]` is the protected
    /// prefix. A synthetic user message that precedes the first real user is NOT
    /// the anchor — only the real prompt anchors the floor. (Carried from
    /// production `apply_compression`'s sacred carve-out, mapped to this flat Vec.)
    pub fn sacred_floor(&self) -> usize {
        // A leading System message is part of the protected prefix.
        let lead_system = usize::from(matches!(
            self.messages.first().map(|m| &m.role),
            Some(Role::System)
        ));
        // Find the FIRST REAL (non-synthetic) user message; the floor extends
        // through it (index + 1). If none exists, the floor is just the lead
        // system (or 0).
        match self
            .messages
            .iter()
            .position(|m| m.role == Role::User && !m.synthetic)
        {
            Some(idx) => idx + 1,
            None => lead_system,
        }
    }

    /// `(context_window, used_tokens, utilization)` from the MOST RECENT assistant
    /// message's recorded `meta` — the provider's last usage report. `(0, 0, 0.0)` when no
    /// assistant turn has been recorded yet (e.g. the first request). The same source the
    /// compaction trigger reads; exposed so a `pre_request` hook can project live context
    /// pressure to the model (e.g. a status reminder) via [`TurnCtx`](crate::hook::TurnCtx).
    pub fn last_pressure(&self) -> (u32, u32, f32) {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.meta.as_ref())
            .map(|meta| (meta.ctx_window, meta.used_tokens, meta.utilization))
            .unwrap_or((0, 0, 0.0))
    }

    /// Prepare a compaction [`CompactionPlan`] without changing this conversation.
    /// (besides `backfill_cancelled_tool_results`). The kernel — not the strategy —
    /// owns and enforces every invariant here, so a buggy strategy cannot corrupt
    /// the conversation:
    ///
    /// 1. REVALIDATE/CLAMP against current state: `drain_from` is clamped up to
    ///    `>= sacred_floor` (never drain the protected prefix); `drain_to` is
    ///    clamped down to `<= messages.len()`; an inverted/empty range
    ///    (`drain_from >= drain_to`) drains nothing (but rewrites / summary-insert /
    ///    resume_note still apply). Out-of-range rewrite indices are SKIPPED
    ///    (never panic).
    /// 2. COMPUTE-THEN-COMMIT for the NET-LOSS GUARD: build the candidate `Vec`
    ///    (drain the range; if `summary` is `Some`, insert ONE
    ///    `Message::synthetic_user(summary)` at `drain_from`; apply rewrites;
    ///    append `resume_note` as a trailing `synthetic_user`), then measure a
    ///    DETERMINISTIC size proxy — per message the bytes that ride the wire
    ///    (`text` + `reasoning` + each `tool_call`'s id/name/arguments + `tool_call_id`),
    ///    summed over all messages — BEFORE vs AFTER. COMMIT only if AFTER is STRICTLY
    ///    smaller than BEFORE. Counting tool-call bytes (not just `text`) is load-bearing:
    ///    a text-light but TOOL-CALL-heavy message (large JSON `arguments`) must register
    ///    as a reduction when dropped, else the strictly-smaller guard would REFUSE a
    ///    genuinely shrinking compaction and a tool-heavy history could never compact.
    /// 3. On a viable candidate: prepare replacement `messages` and bump its
    ///    `cache_epoch += 1` EXACTLY ONCE (decide commit FIRST, bump only after —
    ///    never bump-then-rollback). On REFUSE (not strictly smaller, or noop):
    ///    leave `messages` BYTE-IDENTICAL and do NOT bump `cache_epoch` — a
    ///    refused/no-op compaction never burns a cache epoch.
    ///
    /// This is the append-aware cache contract: a COMMITTED compaction is the ONLY
    /// allowed non-append history change and it opens a new epoch; a REFUSED one
    /// leaves the prefix byte-stable and the epoch unchanged.
    pub(crate) fn prepare_plan(
        &self,
        plan: CompactionPlan,
        sacred_floor: usize,
    ) -> PreparedCompaction {
        // Deterministic size proxy: the bytes that ride the wire for a message — NOT just
        // `text`. Dropping a text-light, TOOL-CALL-heavy message (big JSON arguments) must
        // count as a reduction, else the strictly-smaller net-loss guard below would
        // REFUSE a genuinely shrinking plan and tool-heavy histories could never compact.
        fn size(m: &Message) -> usize {
            let tool_calls: usize = m
                .tool_calls
                .iter()
                .map(|c| c.id.len() + c.name.len() + c.arguments.len())
                .sum();
            m.text.len()
                + m.reasoning.as_ref().map_or(0, |r| r.len())
                + tool_calls
                + m.tool_call_id.as_ref().map_or(0, |id| id.len())
        }
        let epoch_before = self.cache_epoch;
        let bytes_before: usize = self.messages.iter().map(size).sum();
        let len_before = self.messages.len();

        // 1. Clamp the drain range against the protected prefix and current bounds.
        let floor = sacred_floor.min(len_before);
        let drain_from = plan.drain_from.max(floor).min(len_before);
        let drain_to = plan.drain_to.min(len_before);
        // Inverted/empty range → drain nothing.
        let (drain_from, drain_to) = if drain_from >= drain_to {
            (drain_from, drain_from)
        } else {
            (drain_from, drain_to)
        };

        // 2. Build the candidate (compute-then-commit — never mutate self yet).
        let mut candidate: Vec<Message> = Vec::with_capacity(len_before + 2);
        candidate.extend_from_slice(&self.messages[..drain_from]);
        if let Some(summary) = &plan.summary {
            candidate.push(Message::synthetic_user(summary.clone()));
        }
        candidate.extend_from_slice(&self.messages[drain_to..]);
        // Apply rewrites. `rewrites` indices are ORIGINAL `self.messages` indices
        // (the same space as drain_from/drain_to), so each must be TRANSLATED into
        // the candidate Vec before applying. Three guards, in order:
        //   * `orig_i < floor` (PROTECTED PREFIX) → SKIP. The prefix
        //     `candidate[..floor]` equals `messages[..floor]` (drain_from >= floor),
        //     so a `< floor` rewrite would mutate the FROZEN system/first-real-user
        //     prefix — the sacred-floor guarantee must hold for rewrites as for drains.
        //   * `orig_i` inside the drained range `[drain_from, drain_to)` → SKIP. That
        //     message was removed by the drain; there is nothing to rewrite.
        //   * otherwise TRANSLATE to the candidate index:
        //       - `orig_i < drain_from` → unchanged (it precedes the drain).
        //       - `orig_i >= drain_to`  → `drain_from + summary_shift + (orig_i - drain_to)`,
        //         where `summary_shift` is 1 iff a summary was inserted at drain_from.
        // An out-of-range translated index is skipped (never panic).
        let summary_shift = usize::from(plan.summary.is_some());
        for (orig_i, new_text) in &plan.rewrites {
            let orig_i = *orig_i;
            if orig_i < floor {
                continue; // sacred prefix
            }
            if orig_i >= drain_from && orig_i < drain_to {
                continue; // drained away — no surviving message
            }
            let cand_i = if orig_i < drain_from {
                orig_i
            } else {
                // orig_i >= drain_to here (the `[drain_from, drain_to)` case was
                // skipped above). Map past the removed range and the summary insert.
                drain_from + summary_shift + (orig_i - drain_to)
            };
            if let Some(m) = candidate.get_mut(cand_i) {
                m.text = new_text.clone();
            }
        }
        // Append the resume note (if any) as a trailing synthetic user message.
        if let Some(note) = &plan.resume_note {
            candidate.push(Message::synthetic_user(note.clone()));
        }

        // Make the candidate API-VALID before measuring: drop any orphan tool_result
        // left by splitting a tool_call/tool_result pair across the drain boundary,
        // and backfill any dangling assistant tool_call with a `(cancelled)` result
        // (inserted right after its call). The kernel owns this invariant so a buggy
        // strategy cannot corrupt the conversation. Orphan removal counts toward the
        // net-loss measurement (good); if dangling-repair growth makes the candidate
        // not strictly smaller, the plan is correctly REFUSED below (leaving the
        // original, which was valid).
        Self::repair_pairing(&mut candidate);

        let bytes_after: usize = candidate.iter().map(size).sum();

        // 3. Decide commit FIRST; prepare the next epoch only for a real commit.
        let committed = bytes_after < bytes_before;
        let (epoch_after, removed, candidate) = if committed {
            let removed = len_before.saturating_sub(candidate.len());
            let mut candidate = candidate;
            // PRESSURE RELIEF (anti re-fire): the auto task-boundary trigger
            // (`should_compact`) reads the LAST assistant's frozen `meta.used_tokens`
            // and recomputes pressure against the live window. `apply_plan` copies
            // Message structs verbatim and never refreshes meta, so without this the
            // SAME high `used_tokens` would be read at the next boundary and compaction
            // would re-fire (over-shrink / spam Compacted{committed:false}) even though
            // the history is now smaller. Reflect the relieved pressure
            // deterministically: scale the surviving last assistant's `used_tokens`
            // (and `utilization`, kept in lock-step for display) by the byte-reduction
            // ratio `bytes_after / bytes_before` (< 1 here since we committed). This
            // is an estimate that holds until the real provider reports fresh usage
            // on the next turn. Only on commit.
            if bytes_before > 0 {
                let ratio = bytes_after as f64 / bytes_before as f64;
                if let Some(meta) = candidate
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == Role::Assistant)
                    .and_then(|m| m.meta.as_mut())
                {
                    meta.utilization = (meta.utilization as f64 * ratio) as f32;
                    meta.used_tokens = (meta.used_tokens as f64 * ratio).round() as u32;
                }
            }
            let epoch_after = epoch_before + 1;
            (
                epoch_after,
                removed,
                Some(Conversation {
                    messages: candidate,
                    cache_epoch: epoch_after,
                }),
            )
        } else {
            // REFUSE: messages byte-identical, epoch unchanged.
            (epoch_before, 0, None)
        };

        PreparedCompaction {
            candidate,
            report: CompactReport {
                epoch_before,
                epoch_after,
                removed,
                bytes_before,
                bytes_after,
                committed,
            },
        }
    }

    /// Commit a previously prepared candidate with no fallible work.
    pub(crate) fn commit_prepared(&mut self, prepared: PreparedCompaction) -> CompactReport {
        if let Some(candidate) = prepared.candidate {
            *self = candidate;
        }
        prepared.report
    }

    /// Apply a compaction plan immediately.
    ///
    /// Agent-driven manual compaction uses [`Self::prepare_plan`] directly so a
    /// durable checkpoint can be written before this final ownership move. Other
    /// callers retain the original immediate-apply behavior through this wrapper.
    pub fn apply_plan(&mut self, plan: CompactionPlan, sacred_floor: usize) -> CompactReport {
        let prepared = self.prepare_plan(plan, sacred_floor);
        self.commit_prepared(prepared)
    }
}

/// A compaction candidate that passed every kernel invariant but is not live yet.
pub(crate) struct PreparedCompaction {
    candidate: Option<Conversation>,
    report: CompactReport,
}

impl PreparedCompaction {
    pub(crate) fn candidate(&self) -> Option<&Conversation> {
        self.candidate.as_ref()
    }

    pub(crate) fn report(&self) -> CompactReport {
        self.report
    }
}

/// The outcome of [`Conversation::apply_plan`] — a precise audit record of a
/// compaction attempt. `committed == false` means the plan was REFUSED (net-loss
/// guard or noop): `messages` are byte-identical and `epoch_before == epoch_after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactReport {
    pub epoch_before: u64,
    pub epoch_after: u64,
    pub removed: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub committed: bool,
}

/// Why a compaction is being attempted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompactTrigger {
    /// Context-pressure driven: `utilization` of the window has been crossed.
    Auto { utilization: f32 },
    /// User-requested (e.g. `/compact`), optionally focused on a topic.
    Manual { focus: Option<String> },
    /// Hard context-window OVERFLOW recovery (OFF the normal path): the provider rejected
    /// the request as too long. `attempt` (0-based) drives the strategy's escalation
    /// ladder; the kernel increments it per retry. NEVER fired by pressure — only by a
    /// typed overflow error from `chat_stream`.
    Overflow { attempt: u8 },
}

/// READ-ONLY view handed to a [`CompactionStrategy`]: a borrow of the current
/// history plus the pressure facts the kernel already records, and the
/// kernel-computed `sacred_floor`. A strategy reads these to PLAN a drain; it can
/// never mutate the conversation (the kernel is the sole writer via `apply_plan`).
pub struct CompactionView<'a> {
    pub messages: &'a [Message],
    pub trigger: CompactTrigger,
    pub ctx_window: u32,
    pub used_tokens: u32,
    pub utilization: f32,
    /// The number of leading messages the strategy must NOT propose draining (the
    /// kernel re-enforces this anyway by clamping in `apply_plan`).
    pub sacred_floor: usize,
}

/// A strategy's PROPOSAL for one compaction. The kernel REVALIDATES and applies it
/// in [`Conversation::apply_plan`] (clamping, net-loss guard, epoch bump), so a
/// malformed plan can never corrupt invariants.
///
/// Semantics: replace messages in range `[drain_from, drain_to)` with — if
/// `summary` is `Some` — ONE synthetic `Role::User` summary message inserted at
/// `drain_from`; apply `rewrites` as in-place `messages[i].text = new` (for
/// stubbing a tool_result in place — a permanent microcompact); append
/// `resume_note` (if `Some`) as a trailing synthetic `Role::User` message. A NOOP
/// plan is an empty drain range + no summary + no rewrites + no resume_note.
///
/// REWRITE INDEX SPACE (load-bearing): each `rewrites` index is an index into the
/// ORIGINAL `self.messages` Vec (the same space `drain_from`/`drain_to` live in),
/// NOT a post-drain candidate position. `apply_plan` TRANSLATES each original
/// index into the candidate Vec (accounting for the drained range removal and the
/// optional summary-insert shift), so a strategy may combine a drain, a summary,
/// AND a rewrite of a surviving message in ONE plan and still hit the right target.
/// A rewrite whose original index falls inside the drained range `[drain_from,
/// drain_to)` (the message no longer exists) is SILENTLY SKIPPED; one targeting the
/// sacred prefix (`< sacred_floor`) is SKIPPED too.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompactionPlan {
    pub drain_from: usize,
    pub drain_to: usize,
    pub summary: Option<String>,
    pub rewrites: Vec<(usize, String)>,
    pub resume_note: Option<String>,
}

impl CompactionPlan {
    /// The neutral plan: drain nothing, no summary, no rewrites, no resume note.
    pub fn noop() -> Self {
        Self::default()
    }
    /// True iff this plan would change nothing: empty drain range AND no summary AND
    /// no rewrites AND no resume note.
    pub fn is_noop(&self) -> bool {
        self.drain_from >= self.drain_to
            && self.summary.is_none()
            && self.rewrites.is_empty()
            && self.resume_note.is_none()
    }
}

/// The REPLACEABLE compaction POLICY injection point. A single-impl, PLAN-ONLY
/// trait: a strategy only PROPOSES a [`CompactionPlan`] from a read-only
/// [`CompactionView`]; the kernel remains the SOLE writer (via
/// [`Conversation::apply_plan`]), so a buggy strategy cannot corrupt the sacred
/// floor, net-loss, or cache-epoch invariants. Default = [`NoCompaction`] (no-op).
///
/// # PANIC CONTRACT (must-not-panic)
///
/// An implementation **MUST NOT panic**. The kernel does **NOT** isolate panics:
/// under the workspace `panic = "abort"` profile a panic ABORTS THE HOST PROCESS
/// (and `catch_unwind` is a no-op there), and under an unwind profile a panicking
/// strategy is not currently caught either — so a panicking `plan` takes down the
/// whole session / process. Treat all injected code as must-not-panic (the SAME
/// trust posture as the tool-sandbox contract — see [`crate::tool`]): to decline a
/// compaction, return `CompactionPlan::noop()`; never panic.
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan;

    /// CHEAP, side-effect-free pre-check (NO LLM call): will `plan(view)` perform
    /// SLOW, user-visible work — i.e. drain old turns into an LLM summary — rather
    /// than merely no-op or do a fast in-place stub? The kernel calls this to decide
    /// whether to emit [`AgentEvent::CompactionStarted`] (the "compacting…" progress
    /// line), so a manual `/compact` that turns out to be a no-op never shows a
    /// spurious "compacting…" line ahead of "nothing to compact". Default `false`
    /// (e.g. [`NoCompaction`] and pure-stub policies never summarize).
    fn will_summarize(&self, _view: &CompactionView<'_>) -> bool {
        false
    }
}

/// The neutral DEFAULT strategy: never compacts (always returns a noop plan).
pub struct NoCompaction;

#[async_trait]
impl CompactionStrategy for NoCompaction {
    async fn plan(&self, _view: &CompactionView<'_>) -> CompactionPlan {
        CompactionPlan::noop()
    }
}

/// On-disk/over-the-wire schema version for a persisted conversation. Bump it
/// whenever the serialized shape of `Message`/`Conversation` changes in a way an
/// older kernel could not read. A reader checks this BEFORE interpreting
/// `messages`, so a session written by one kernel version is never silently
/// misread by another.
pub const SNAPSHOT_VERSION: u32 = 1;

/// A versioned, LOSSLESS, resumable conversation snapshot — the durable contract
/// for persisting and resuming a session.
///
/// `version` is the FORWARD-COMPAT SEAM: a resumer compares it against
/// `SNAPSHOT_VERSION` and only interprets `messages` if it can. Carrying the full
/// `Vec<Message>` (not a lossy summary) means `tool_calls`, `tool_call_id`, and
/// `meta` all survive — so a resumed session continues append-only and the
/// provider's prefix cache stays warm across the resume boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub messages: Vec<Message>,
    /// The conversation's PREFIX-GENERATION marker, persisted so a resumed session
    /// preserves which cache epoch it was on. ADDITIVE: `#[serde(default)]` keeps a
    /// v1 snapshot (no `cache_epoch` field) loadable (→ 0). `new(messages)`
    /// defaults it to 0; `from_conversation` copies the live value.
    #[serde(default)]
    pub cache_epoch: u64,
    /// ID HIGH-WATER MARKS: how many `turn_id`s / `request_id`s the session had
    /// minted when this snapshot was taken. A resume seeds the kernel's counters
    /// from these so a resumed session CONTINUES the monotonic id sequence instead
    /// of restarting at 1 — without this, an append-only per-session transcript
    /// keyed by `(session_id, turn_id)` collects duplicate keys after the first
    /// resume. ADDITIVE (`#[serde(default)]` → 0); the resume path additionally
    /// falls back to the max `meta.turn_id`/`meta.request_id` over `messages`, so
    /// even an OLD snapshot without these fields resumes monotonically.
    #[serde(default)]
    pub turn_counter: u64,
    #[serde(default)]
    pub request_counter: u64,
}

impl SessionSnapshot {
    /// Stamp the current `SNAPSHOT_VERSION` over the given messages (epoch 0,
    /// counters derived from the messages' metas).
    pub fn new(messages: Vec<Message>) -> Self {
        let (turn_counter, request_counter) = Self::derive_counters(&messages);
        Self {
            version: SNAPSHOT_VERSION,
            messages,
            cache_epoch: 0,
            turn_counter,
            request_counter,
        }
    }
    /// Snapshot a live conversation losslessly at the current version, carrying its
    /// `cache_epoch` so a resume restores the same prefix generation. The id
    /// high-water marks are DERIVED from the stored metas — exact whenever every
    /// turn stored at least one assistant message; a capturer that knows the live
    /// counters (e.g. a `turn_complete` hook holding `TurnCtx`) may bump them
    /// higher for turns that died before any response was stored.
    pub fn from_conversation(convo: &Conversation) -> Self {
        let (turn_counter, request_counter) = Self::derive_counters(&convo.messages);
        Self {
            version: SNAPSHOT_VERSION,
            messages: convo.messages.clone(),
            cache_epoch: convo.cache_epoch,
            turn_counter,
            request_counter,
        }
    }
    /// Max `meta.turn_id` / `meta.request_id` over the messages (0 when none carry meta).
    pub fn derive_counters(messages: &[Message]) -> (u64, u64) {
        messages
            .iter()
            .filter_map(|m| m.meta.as_ref())
            .fold((0, 0), |(t, r), meta| {
                (t.max(meta.turn_id), r.max(meta.request_id))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_pressure_reads_latest_assistant_meta() {
        let mut c = Conversation::new();
        c.push(Message::user("hi"));
        assert_eq!(
            c.last_pressure(),
            (0, 0, 0.0),
            "no assistant meta yet → zeros"
        );
        let mut a = Message::assistant("ans", vec![]);
        a.meta = Some(MessageMeta {
            ctx_window: 128_000,
            used_tokens: 40_000,
            utilization: 0.3125,
            ..Default::default()
        });
        c.push(a);
        assert_eq!(c.last_pressure(), (128_000, 40_000, 0.3125));
    }

    #[test]
    fn conversation_records_messages_in_order() {
        let mut c = Conversation::new();
        c.push(Message::user("hi"));
        c.push(Message::assistant("hello", vec![]));
        assert_eq!(c.messages.len(), 2);
        assert!(matches!(c.messages[0].role, Role::User));
        assert_eq!(c.messages[0].text, "hi");
        assert!(matches!(c.messages[1].role, Role::Assistant));

        let tr = Message::tool_result("call-1", "output", false);
        assert!(matches!(tr.role, Role::Tool));
        assert_eq!(tr.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(tr.text, "output");
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn user_with_images_carries_them_else_empty() {
        let m = Message::user_with_images(
            "hi",
            vec![ImageContent {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        assert_eq!(m.images.len(), 1);
        assert_eq!(m.images[0].media_type, "image/png");
        assert!(
            Message::user("hi").images.is_empty(),
            "a plain user message has no images"
        );
    }

    #[test]
    fn message_serde_is_additive_for_images() {
        // An OLD snapshot message (no `images` field) must still deserialize → empty.
        let old = r#"{"role":"User","text":"hi","tool_calls":[],"tool_call_id":null,"is_error":false,"meta":null}"#;
        let m: Message = serde_json::from_str(old).unwrap();
        assert!(
            m.images.is_empty(),
            "missing images field defaults to empty"
        );
        // A round-trip with images preserves them losslessly.
        let with = Message::user_with_images(
            "x",
            vec![ImageContent {
                media_type: "image/png".into(),
                data: "AA".into(),
            }],
        );
        let back: Message = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back.images, with.images);
    }

    #[test]
    fn reasoning_blocks_default_empty_and_serde_additive() {
        // A plain assistant message has no signed reasoning blocks.
        assert!(
            Message::assistant("ans", vec![])
                .reasoning_blocks
                .is_empty(),
            "a plain assistant message carries no reasoning_blocks"
        );
        // An OLD snapshot (no `reasoning_blocks` field) must still deserialize → empty.
        let old = r#"{"role":"Assistant","text":"ans","tool_calls":[],"tool_call_id":null,"is_error":false,"meta":null}"#;
        let m: Message = serde_json::from_str(old).unwrap();
        assert!(
            m.reasoning_blocks.is_empty(),
            "missing reasoning_blocks defaults to empty"
        );
        // A round-trip with blocks preserves text + opaque + provider losslessly.
        let mut with = Message::assistant("ans", vec![]);
        with.reasoning_blocks = vec![
            ReasoningBlock {
                text: "let me think".into(),
                opaque: Some("sig-abc".into()),
                provider: Some("anthropic".into()),
            },
            // a redacted block: no readable text, opaque only.
            ReasoningBlock {
                text: String::new(),
                opaque: Some("redacted-data".into()),
                provider: Some("anthropic".into()),
            },
        ];
        let back: Message = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back.reasoning_blocks, with.reasoning_blocks);
    }

    // Mirrors production `cancel_backfills_missing_tool_results`: an assistant
    // message carrying 2 tool_calls and NO results → after backfill there are 2
    // tool-result messages, each "(cancelled)" / is_error=true, matching the
    // two call_ids; and existing messages are untouched (append-only).
    #[test]
    fn cancel_backfills_missing_tool_results() {
        let mut c = Conversation::new();
        c.push(Message::user("do two things"));
        c.push(Message::assistant(
            "calling",
            vec![call("call_1", "write_file"), call("call_2", "echo")],
        ));
        let before = c.messages.clone();
        assert_eq!(c.messages.len(), 2);

        c.backfill_cancelled_tool_results();

        // Append-only: original messages unchanged, two results appended.
        assert_eq!(c.messages.len(), 4);
        for (orig, now) in before.iter().zip(c.messages.iter()) {
            assert_eq!(orig.role, now.role);
            assert_eq!(orig.text, now.text);
            assert_eq!(orig.tool_calls, now.tool_calls);
            assert_eq!(orig.tool_call_id, now.tool_call_id);
        }

        let results: Vec<&Message> = c.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(results.len(), 2, "exactly two (cancelled) results appended");
        let ids: Vec<&str> = results
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert!(ids.contains(&"call_1"));
        assert!(ids.contains(&"call_2"));
        for r in &results {
            assert_eq!(r.text, "(cancelled)");
            assert_eq!(r.role, Role::Tool);
        }
    }

    // Mirrors production
    // `cancel_preserves_completed_tool_pairs_and_backfills_incomplete`: an
    // assistant with 2 tool_calls where ONE already has a real result → backfill
    // adds EXACTLY ONE "(cancelled)" result for the missing one; the real result
    // is untouched; no duplicates.
    #[test]
    fn cancel_preserves_completed_pairs_and_backfills_incomplete() {
        let mut c = Conversation::new();
        c.push(Message::user("read then edit"));
        c.push(Message::assistant(
            "read",
            vec![call("call_1", "read_file")],
        ));
        c.push(Message::tool_result("call_1", "fn main() {}", false));
        c.push(Message::assistant(
            "edit",
            vec![call("call_2", "edit_file")],
        ));
        let before = c.messages.clone();
        assert_eq!(c.messages.len(), 4);

        c.backfill_cancelled_tool_results();

        // Exactly one result appended (for call_2); the rest unchanged.
        assert_eq!(c.messages.len(), 5);
        for (orig, now) in before.iter().zip(c.messages.iter()) {
            assert_eq!(orig.text, now.text);
            assert_eq!(orig.tool_call_id, now.tool_call_id);
        }
        // The real result for call_1 is untouched (still success / real output).
        assert_eq!(c.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(c.messages[2].text, "fn main() {}");
        // The single backfilled result is for call_2.
        let appended = &c.messages[4];
        assert_eq!(appended.role, Role::Tool);
        assert_eq!(appended.tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(appended.text, "(cancelled)");
        // No duplicate result for call_1 (only one Tool message references it).
        let call1_results = c
            .messages
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .count();
        assert_eq!(
            call1_results, 1,
            "no duplicate (cancelled) for the completed call"
        );
    }

    // Backfill is idempotent: once every call has a result, a second call adds
    // nothing.
    #[test]
    fn backfill_is_idempotent_when_all_paired() {
        let mut c = Conversation::new();
        c.push(Message::assistant("x", vec![call("call_1", "echo")]));
        c.backfill_cancelled_tool_results();
        let len = c.messages.len();
        assert_eq!(len, 2);
        c.backfill_cancelled_tool_results();
        assert_eq!(
            c.messages.len(),
            len,
            "no second (cancelled) for an already-paired call"
        );
    }

    // A full Conversation — system + user + assistant-with-tool_calls + tool_result
    // (with tool_call_id/is_error) + a message carrying `meta` — survives a
    // serde_json round-trip BYTE-FOR-FIELD identically (PartialEq). This is the
    // losslessness contract the OLD `MessageSnapshot` violated: it dropped
    // `tool_calls` and `tool_call_id` and stringified `Role` via Debug.
    #[test]
    fn conversation_serde_roundtrip_is_lossless() {
        let mut c = Conversation::new();
        c.push(Message::system("you are neutral"));
        c.push(Message::user("read the file then summarize"));
        // assistant message carrying TWO tool_calls (id / name / arguments).
        c.push(Message::assistant(
            "calling tools",
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"/x\"}".into(),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "grep".into(),
                    arguments: "{\"q\":\"foo\"}".into(),
                },
            ],
        ));
        // a tool_result with tool_call_id set and is_error=true.
        c.push(Message::tool_result("call_1", "boom", true));
        // a message carrying a non-default `meta` sidecar AND stored `reasoning`
        // (a thinking model's prior-turn thinking, which a provider adapter echoes
        // back next turn — the kernel stores it losslessly here).
        let mut with_meta = Message::assistant("done", vec![]);
        with_meta.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 50,
                completion: 7,
                cached: 3,
            },
            elapsed_ms: 123,
            reasoning_elapsed_ms: 0,
            ctx_window: 1000,
            used_tokens: 50,
            utilization: 0.05,
            round: 2,
            turn_id: 1,
            request_id: 2,
            provider_response_id: Some("resp_abc".into()),
            provider_model: Some("deepseek-v4-flash".into()),
            session_id: Some("sess-1".into()),
            finish_reason: "stop".into(),
        });
        with_meta.reasoning = Some("thinking…".to_string());
        c.push(with_meta);

        let json = serde_json::to_string(&c).expect("Conversation must serialize");
        let back: Conversation =
            serde_json::from_str(&json).expect("Conversation must deserialize");

        // Whole-conversation equality proves NOTHING was dropped or mangled.
        assert_eq!(
            back, c,
            "round-trip must be lossless (Conversation PartialEq)"
        );

        // Spell out the bits the OLD lossy MessageSnapshot silently dropped, so a
        // regression to a lossy projection fails LOUDLY here:
        let asst = &back.messages[2];
        assert_eq!(
            asst.tool_calls.len(),
            2,
            "tool_calls must survive the round-trip"
        );
        assert_eq!(asst.tool_calls[0].id, "call_1");
        assert_eq!(asst.tool_calls[0].name, "read_file");
        assert_eq!(asst.tool_calls[0].arguments, "{\"path\":\"/x\"}");
        assert_eq!(asst.tool_calls[1].id, "call_2");

        let tr = &back.messages[3];
        assert_eq!(
            tr.tool_call_id.as_deref(),
            Some("call_1"),
            "tool_call_id must survive"
        );
        assert_eq!(tr.text, "boom");
        // is_error is a REAL semantic property (a real adapter echoes it to the
        // provider). It was silently dropped before; assert it now survives.
        assert!(
            tr.is_error,
            "tool_result is_error must survive the round-trip"
        );

        assert!(back.messages[4].meta.is_some(), "meta sidecar must survive");
        assert_eq!(back.messages[4].meta.as_ref().unwrap().round, 2);

        // The stored reasoning survives the round-trip (a provider adapter echoes
        // it back next turn; the kernel only stores it). It was DROPPED before
        // `Message.reasoning` existed.
        assert_eq!(
            back.messages[4].reasoning.as_deref(),
            Some("thinking…"),
            "stored reasoning must survive the round-trip"
        );
        // A non-thinking / non-assistant message has no reasoning.
        assert_eq!(
            back.messages[1].reasoning, None,
            "user message has no reasoning"
        );
        assert_eq!(
            back.messages[2].reasoning, None,
            "assistant w/o thinking has None"
        );
    }

    // ADDITIVE serde-default: a v1-style assistant message JSON WITHOUT a
    // `reasoning` field still deserializes (serde default → None), so an older
    // snapshot written before `Message.reasoning` existed is still readable.
    #[test]
    fn message_without_reasoning_field_defaults_to_none() {
        // No "reasoning" key — exactly what a v1 kernel wrote.
        let v1 = r#"{"role":"Assistant","text":"answer","tool_calls":[],"tool_call_id":null,"is_error":false,"meta":null,"synthetic":false}"#;
        let m: Message =
            serde_json::from_str(v1).expect("v1 message (no reasoning) must deserialize");
        assert_eq!(
            m.reasoning, None,
            "missing reasoning field defaults to None"
        );
        assert_eq!(m.text, "answer");

        // And a stored Some(..) reasoning serializes + round-trips standalone.
        let mut a = Message::assistant("ans", vec![]);
        a.reasoning = Some("let me think".into());
        let json = serde_json::to_string(&a).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning.as_deref(), Some("let me think"));
        assert_eq!(back, a, "Message with reasoning round-trips losslessly");
    }

    #[test]
    fn message_without_internal_origin_field_defaults_to_none() {
        let v1 = r#"{"role":"Assistant","text":"answer","tool_calls":[],"tool_call_id":null,"is_error":false,"meta":null,"synthetic":false}"#;
        let m: Message =
            serde_json::from_str(v1).expect("v1 message (no internal_origin) must deserialize");
        assert!(m.internal_origin.is_none());

        let mut a = Message::assistant("No verification is needed.", vec![]);
        a.internal_origin = Some("verify_cadence".into());
        let json = serde_json::to_string(&a).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.internal_origin.as_deref(), Some("verify_cadence"));
    }

    // `Role` serializes to its STABLE variant tag — the derived enum name is the
    // wire contract now (NOT a `{:?}` Debug artifact) — and round-trips.
    #[test]
    fn role_serializes_to_stable_tag() {
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"Assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"System\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"User\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"Tool\"");
        let back: Role = serde_json::from_str("\"Assistant\"").unwrap();
        assert_eq!(back, Role::Assistant);
    }

    // The versioned envelope stamps the current SNAPSHOT_VERSION and carries the
    // full lossless messages; it round-trips and `from_conversation` mirrors the
    // conversation's messages exactly.
    #[test]
    fn session_snapshot_is_versioned_and_round_trips() {
        let mut c = Conversation::new();
        c.push(Message::system("persona"));
        c.push(Message::user("hi"));

        let snap = SessionSnapshot::from_conversation(&c);
        assert_eq!(
            snap.version, SNAPSHOT_VERSION,
            "constructor stamps the current version"
        );
        assert_eq!(
            snap.messages, c.messages,
            "from_conversation carries messages losslessly"
        );

        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap, "SessionSnapshot round-trips");

        // `new` stamps the version too.
        let snap2 = SessionSnapshot::new(c.messages.clone());
        assert_eq!(snap2.version, SNAPSHOT_VERSION);
        assert_eq!(snap2.messages, c.messages);
    }

    // ── compaction MECHANISM (kernel L0) ──────────────────────────────────

    // sacred_floor protects a leading System message PLUS up to and including the
    // FIRST NON-SYNTHETIC user message. A synthetic user message that precedes the
    // first real user is NOT the anchor — only the real prompt anchors the floor.
    #[test]
    fn sacred_floor_protects_system_and_first_real_user() {
        let mut c = Conversation::new();
        c.push(Message::system("persona"));
        c.push(Message::user("task")); // first REAL user message → index 1
        c.push(Message::assistant("ok", vec![]));
        c.push(Message::user("more"));
        // floor = system(1) + through first real user(idx 1) = count 2.
        assert_eq!(c.sacred_floor(), 2);

        // A synthetic user BEFORE the first real user must not be the anchor.
        let mut c2 = Conversation::new();
        c2.push(Message::system("persona"));
        c2.push(Message::synthetic_user("[resume note]")); // synthetic, NOT anchor
        c2.push(Message::user("real task")); // first REAL user → index 2
        c2.push(Message::assistant("ok", vec![]));
        // floor must extend through the first REAL user (index 2) → count 3.
        assert_eq!(c2.sacred_floor(), 3);

        // No system, real user first.
        let mut c3 = Conversation::new();
        c3.push(Message::user("hi"));
        c3.push(Message::assistant("ho", vec![]));
        assert_eq!(c3.sacred_floor(), 1);
    }

    // Draining a middle range with a summary: messages shrink, ONE synthetic
    // Role::User summary appears at drain_from, system[0] byte-identical, epoch +1,
    // committed.
    #[test]
    fn apply_plan_drains_and_inserts_synthetic_summary_and_bumps_epoch() {
        let mut c = Conversation::new();
        c.push(Message::system("PERSONA-FROZEN"));
        c.push(Message::user("the task"));
        c.push(Message::assistant("aaaaaaaaaa", vec![]));
        c.push(Message::tool_result("c1", "bbbbbbbbbb", false));
        c.push(Message::assistant("cccccccccc", vec![]));
        c.push(Message::user("dddddddddd"));
        let floor = c.sacred_floor(); // 2
        let sys_before = c.messages[0].clone();
        let epoch_before = c.cache_epoch;

        // Drain [2,5): the three middle messages → replace with a short summary.
        let plan = CompactionPlan {
            drain_from: 2,
            drain_to: 5,
            summary: Some("summary".into()),
            rewrites: vec![],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);

        assert!(report.committed, "net-smaller plan must commit");
        assert_eq!(c.cache_epoch, epoch_before + 1, "epoch bumps by exactly 1");
        assert_eq!(report.epoch_before, epoch_before);
        assert_eq!(report.epoch_after, epoch_before + 1);
        // Was 6 messages; drained 3, inserted 1 summary → 4.
        assert_eq!(c.messages.len(), 4);
        // system unchanged byte-for-byte.
        assert_eq!(c.messages[0], sys_before);
        // sacred user prompt survives.
        assert_eq!(c.messages[1].text, "the task");
        // ONE synthetic Role::User summary at drain_from.
        let s = &c.messages[2];
        assert_eq!(s.role, Role::User);
        assert!(s.synthetic, "summary must be synthetic");
        assert_eq!(s.text, "summary");
        // trailing real user message preserved.
        assert_eq!(c.messages[3].text, "dddddddddd");
        assert!(report.removed > 0);
        assert!(report.bytes_after < report.bytes_before);
    }

    // The size proxy counts TOOL-CALL bytes, not just `text`: dropping a text-light but
    // tool-call-heavy message for a short summary is a NET REDUCTION and must commit (a
    // text-only proxy would refuse it, so tool-heavy histories could never compact).
    #[test]
    fn apply_plan_size_proxy_counts_tool_call_bytes_so_tool_heavy_summary_commits() {
        let big_args = format!("{{\"data\":\"{}\"}}", "x".repeat(500));
        let mut c = Conversation::new();
        c.push(Message::system("sys"));
        c.push(Message::user("task"));
        c.push(Message::assistant(
            "", // text-light…
            vec![crate::tool::ToolCall {
                id: "c1".into(),
                name: "t".into(),
                arguments: big_args,
            }], // …tool-call-heavy
        ));
        c.push(Message::tool_result("c1", "ok", false));
        c.push(Message::user("next"));
        let floor = c.sacred_floor(); // 2 (system + first user)

        // Drain the tool-heavy assistant+result pair [2,4), replace with a 100-char
        // summary. Text-only proxy: ~0 drained text vs +100 summary → would REFUSE.
        // Counting the ~500-byte tool arguments makes it a clear net reduction.
        let plan = CompactionPlan {
            drain_from: 2,
            drain_to: 4,
            summary: Some("S".repeat(100)),
            rewrites: vec![],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);

        assert!(
            report.committed,
            "dropping a tool-call-heavy message for a short summary must be a net reduction (tool_call bytes counted), not refused by a text-only proxy"
        );
        assert!(report.bytes_before > report.bytes_after);
    }

    // A plan whose result is NOT smaller (summary longer than what it replaces, or
    // noop) → messages byte-identical AND cache_epoch UNCHANGED AND committed=false.
    #[test]
    fn apply_plan_refuses_net_loss_and_does_not_bump_epoch() {
        let mut c = Conversation::new();
        c.push(Message::system("sys"));
        c.push(Message::user("task"));
        c.push(Message::assistant("x", vec![])); // tiny middle
        c.push(Message::user("y"));
        let floor = c.sacred_floor();
        let before = c.messages.clone();
        let epoch_before = c.cache_epoch;

        // Summary far longer than the 1-byte "x" it replaces → NOT net smaller.
        let plan = CompactionPlan {
            drain_from: 2,
            drain_to: 3,
            summary: Some("a very long summary that is bigger".into()),
            rewrites: vec![],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);
        assert!(!report.committed, "net-loss plan must REFUSE");
        assert_eq!(c.messages, before, "messages byte-identical on refuse");
        assert_eq!(c.cache_epoch, epoch_before, "epoch UNCHANGED on refuse");

        // A noop plan also refuses and never burns an epoch.
        let report2 = c.apply_plan(CompactionPlan::noop(), floor);
        assert!(!report2.committed);
        assert_eq!(c.messages, before);
        assert_eq!(c.cache_epoch, epoch_before, "noop never bumps epoch");
    }

    // A plan with drain_from=0 (trying to remove system/first-user) → clamped to
    // sacred_floor; the protected prefix survives byte-identical.
    #[test]
    fn apply_plan_never_drains_below_sacred_floor() {
        let mut c = Conversation::new();
        c.push(Message::system("SYS-FROZEN"));
        c.push(Message::user("TASK-FROZEN"));
        c.push(Message::assistant("mmmmmmmmmm", vec![]));
        c.push(Message::tool_result("c1", "nnnnnnnnnn", false));
        c.push(Message::user("oooooooooo"));
        let floor = c.sacred_floor(); // 2
        let sys = c.messages[0].clone();
        let task = c.messages[1].clone();

        // Try to drain from 0 across the whole prefix — must be clamped to floor.
        let plan = CompactionPlan {
            drain_from: 0,
            drain_to: 4,
            summary: Some("s".into()),
            rewrites: vec![],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);
        assert!(report.committed);
        // protected prefix survives byte-identical, still at indices 0,1.
        assert_eq!(c.messages[0], sys);
        assert_eq!(c.messages[1], task);
        // summary inserted at the floor, not before it.
        assert_eq!(c.messages[2].role, Role::User);
        assert!(c.messages[2].synthetic);
        assert_eq!(c.messages[2].text, "s");
    }

    // A rewrite of a tool_result's text → that message's text changes, others
    // untouched, epoch bumps iff net smaller. (Fixture is API-valid: the assistant
    // carries the `c1` tool_call that the tool_result pairs with, so the kernel's
    // pair-validity repair is a no-op here and the in-place rewrite is what's tested.)
    #[test]
    fn apply_plan_rewrites_in_place_are_applied() {
        let mut c = Conversation::new();
        c.push(Message::system("sys"));
        c.push(Message::user("task"));
        c.push(Message::assistant("call", vec![call("c1", "read_file")]));
        c.push(Message::tool_result(
            "c1",
            "HUGE TOOL OUTPUT THAT IS LONG",
            false,
        ));
        c.push(Message::user("next"));
        let floor = c.sacred_floor();
        let epoch_before = c.cache_epoch;

        // Rewrite the tool_result (idx 3) in place to a short stub; no drain.
        let plan = CompactionPlan {
            drain_from: 0,
            drain_to: 0,
            summary: None,
            rewrites: vec![(3, "[stubbed]".into())],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);
        assert!(report.committed, "rewrite that shrinks bytes must commit");
        assert_eq!(c.cache_epoch, epoch_before + 1);
        assert_eq!(c.messages[3].text, "[stubbed]");
        assert_eq!(
            c.messages[3].tool_call_id.as_deref(),
            Some("c1"),
            "other fields untouched"
        );
        // siblings untouched.
        assert_eq!(c.messages[2].text, "call");
        assert_eq!(c.messages[4].text, "next");
        assert_eq!(c.messages.len(), 5, "rewrite does not change count");

        // An out-of-range rewrite index is skipped (never panics) and, alone, is a
        // no-op → refuse.
        let mut c2 = c.clone();
        let epoch2 = c2.cache_epoch;
        let report2 = c2.apply_plan(
            CompactionPlan {
                drain_from: 0,
                drain_to: 0,
                summary: None,
                rewrites: vec![(999, "x".into())],
                resume_note: None,
            },
            c2.sacred_floor(),
        );
        assert!(
            !report2.committed,
            "out-of-range rewrite that changes nothing must refuse"
        );
        assert_eq!(c2.cache_epoch, epoch2);
    }

    // Snapshot serde round-trip preserves cache_epoch and synthetic; and a v1-style
    // JSON WITHOUT those fields still deserializes via serde default → epoch 0,
    // synthetic false.
    #[test]
    fn snapshot_round_trips_cache_epoch_and_synthetic() {
        let mut c = Conversation::new();
        c.cache_epoch = 3;
        c.push(Message::system("sys"));
        c.push(Message::user("task"));
        c.push(Message::synthetic_user("[cold summary]"));

        let snap = SessionSnapshot::from_conversation(&c);
        assert_eq!(snap.cache_epoch, 3, "from_conversation copies cache_epoch");
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cache_epoch, 3, "cache_epoch survives round-trip");
        assert!(back.messages[2].synthetic, "synthetic survives round-trip");
        assert!(!back.messages[0].synthetic);

        // A v1-style JSON with NO cache_epoch / synthetic fields still loads
        // (serde default → epoch 0, synthetic false).
        let v1 = r#"{"version":1,"messages":[{"role":"User","text":"hi","tool_calls":[],"tool_call_id":null,"is_error":false,"meta":null}]}"#;
        let loaded: SessionSnapshot =
            serde_json::from_str(v1).expect("v1 snapshot must still deserialize");
        assert_eq!(loaded.cache_epoch, 0, "missing cache_epoch defaults to 0");
        assert!(
            !loaded.messages[0].synthetic,
            "missing synthetic defaults to false"
        );

        // `new` defaults epoch 0.
        let snap2 = SessionSnapshot::new(vec![Message::user("x")]);
        assert_eq!(snap2.cache_epoch, 0);
    }

    // ── BLOCKER 1 — pair-validity (orphan scrub + dangling repair) ───────────

    /// Returns Err(reason) if `msgs` is NOT API-pair-valid: every assistant
    /// `tool_call.id` must have exactly one following `tool_result` carrying that
    /// `tool_call_id`, and every `tool_result`'s `tool_call_id` must have a
    /// PRECEDING assistant `tool_call`.
    fn check_pair_valid(msgs: &[Message]) -> Result<(), String> {
        // Every assistant tool_call → exactly one FOLLOWING result with that id.
        for (i, m) in msgs.iter().enumerate() {
            if m.role == Role::Assistant {
                for tc in &m.tool_calls {
                    let following = msgs[i + 1..]
                        .iter()
                        .filter(|r| r.tool_call_id.as_deref() == Some(tc.id.as_str()))
                        .count();
                    if following != 1 {
                        return Err(format!(
                            "tool_call {} has {} following results (want exactly 1)",
                            tc.id, following
                        ));
                    }
                }
            }
        }
        // Every tool_result → a PRECEDING assistant tool_call with that id.
        for (i, m) in msgs.iter().enumerate() {
            if m.role == Role::Tool {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                let preceded = msgs[..i].iter().any(|a| {
                    a.role == Role::Assistant && a.tool_calls.iter().any(|tc| tc.id == id)
                });
                if !preceded {
                    return Err(format!(
                        "tool_result {id} is an ORPHAN (no preceding tool_call)"
                    ));
                }
            }
        }
        Ok(())
    }

    // A history with interleaved tool_call/tool_result pairs: for EVERY keep_recent,
    // SummarizeOldest's plan applied via apply_plan must leave PAIR-VALID messages —
    // no orphan tool_result (pair split by the drain) and no dangling tool_call.
    // (This is the probe the reviewer used against the real bug, made permanent.)
    #[tokio::test]
    async fn apply_plan_never_leaves_orphan_or_dangling_for_any_keep_recent() {
        use crate::testkit::SummarizeOldestStrategy;
        let base: Vec<Message> = vec![
            Message::system("PERSONA"),
            Message::user("the task"),
            Message::assistant("call one", vec![call("c1", "echo")]),
            Message::tool_result("c1", "result one is fairly long here", false),
            Message::user("an interleaved user note"),
            Message::assistant("call two", vec![call("c2", "echo")]),
            Message::tool_result("c2", "result two is also fairly long", false),
            Message::assistant("call three", vec![call("c3", "echo")]),
            Message::tool_result("c3", "result three padding padding", false),
            Message::assistant("final answer with some length to it", vec![]),
        ];
        let len = base.len();
        for keep_recent in 0..=len {
            let mut c = Conversation {
                messages: base.clone(),
                cache_epoch: 0,
            };
            let floor = c.sacred_floor();
            let view = CompactionView {
                messages: &c.messages,
                trigger: CompactTrigger::Manual { focus: None },
                ctx_window: 1000,
                used_tokens: 0,
                utilization: 0.0,
                sacred_floor: floor,
            };
            let plan = SummarizeOldestStrategy { keep_recent }.plan(&view).await;
            c.apply_plan(plan, floor);
            check_pair_valid(&c.messages).unwrap_or_else(|e| {
                panic!(
                    "keep_recent={keep_recent} produced an INVALID pairing: {e}\n{:#?}",
                    c.messages
                )
            });
        }
    }

    // repair_pairing in isolation: it DROPS an orphan tool_result and BACKFILLS a
    // dangling tool_call (inserting the stub right AFTER its assistant call).
    #[test]
    fn repair_pairing_drops_orphan_and_backfills_dangling_in_order() {
        // Orphan: a tool_result whose call was removed. Dangling: an assistant call
        // with no result.
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("task"),
            Message::tool_result("orphan", "leftover result", false), // ORPHAN
            Message::assistant("calling", vec![call("dang", "echo")]), // DANGLING
            Message::user("trailing"),
        ];
        Conversation::repair_pairing(&mut msgs);
        check_pair_valid(&msgs).expect("must be pair-valid after repair");
        // Orphan gone.
        assert!(
            !msgs
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("orphan")),
            "orphan tool_result must be dropped"
        );
        // Dangling backfilled with a (cancelled) result RIGHT AFTER its call.
        let dang_call_idx = msgs
            .iter()
            .position(|m| m.tool_calls.iter().any(|tc| tc.id == "dang"))
            .unwrap();
        let after = &msgs[dang_call_idx + 1];
        assert_eq!(after.role, Role::Tool);
        assert_eq!(after.tool_call_id.as_deref(), Some("dang"));
        assert_eq!(after.text, "(cancelled)");
        assert!(after.is_error);
    }

    // repair_pairing is a NO-OP on an already-valid history (the orphan scrub /
    // dangling repair must not perturb well-formed pairs — claim 22/23 safety).
    #[test]
    fn repair_pairing_is_noop_on_valid_history() {
        let valid = vec![
            Message::system("sys"),
            Message::user("task"),
            Message::assistant("call", vec![call("c1", "echo")]),
            Message::tool_result("c1", "out", false),
            Message::assistant("done", vec![]),
        ];
        let mut msgs = valid.clone();
        Conversation::repair_pairing(&mut msgs);
        assert_eq!(
            msgs, valid,
            "repair_pairing must be a no-op on an already-valid history"
        );
    }

    // ── BLOCKER 2 — rewrites respect the sacred floor ────────────────────────

    // A rewrite targeting an index in the protected prefix (system idx 0 and the
    // first real user idx 1) is IGNORED; an in-range rewrite still applies.
    #[test]
    fn apply_plan_rewrite_cannot_touch_sacred_prefix() {
        let mut c = Conversation::new();
        c.push(Message::system("SYSTEM-PERSONA"));
        c.push(Message::user("THE-REAL-TASK"));
        c.push(Message::assistant(
            "middle that is long enough to shrink",
            vec![],
        ));
        c.push(Message::user("tail"));
        let floor = c.sacred_floor(); // 2
        let sys_before = c.messages[0].clone();
        let user_before = c.messages[1].clone();
        let epoch_before = c.cache_epoch;

        // Rewrites at idx 0 (system) and idx 1 (first real user) MUST be ignored;
        // the in-range rewrite at idx 2 (shrinking the middle) applies → commit.
        let plan = CompactionPlan {
            drain_from: 0,
            drain_to: 0,
            summary: None,
            rewrites: vec![
                (0, "HACKED".into()),
                (1, "HACKED".into()),
                (2, "short".into()),
            ],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);
        assert!(
            report.committed,
            "the in-range shrinking rewrite must commit"
        );
        assert_eq!(c.cache_epoch, epoch_before + 1);
        // Sacred prefix BYTE-IDENTICAL (rewrites ignored).
        assert_eq!(c.messages[0], sys_before, "system must be byte-identical");
        assert_eq!(
            c.messages[1], user_before,
            "first real user must be byte-identical"
        );
        // The in-range rewrite applied.
        assert_eq!(c.messages[2].text, "short");
    }

    // ── BUG 1 — rewrite-index space combines drain + summary + rewrite ───────

    // The load-bearing combining case: a plan that DRAINS a middle range, inserts a
    // SUMMARY, AND rewrites a message ORIGINALLY AFTER drain_to. Rewrite indices are
    // ORIGINAL `self.messages` indices, so `apply_plan` must TRANSLATE them
    // (accounting for the removed range + the summary-insert shift). Assert:
    //   * the rewrite targeting an ORIGINAL index after drain_to hits the CORRECT
    //     surviving message (not an off-by-N neighbor),
    //   * a rewrite targeting a DRAINED index is silently skipped,
    //   * a rewrite targeting the sacred prefix is skipped.
    #[test]
    fn apply_plan_rewrite_index_translates_with_drain_and_summary() {
        let mut c = Conversation::new();
        c.push(Message::system("SYS-FROZEN")); // 0 sacred
        c.push(Message::user("THE-TASK")); // 1 sacred (first real user) → floor 2
                                           // Drained pair (idx 2,3): assistant call c1 + its result, both inside [2,4).
        c.push(Message::assistant(
            "drain me A long enough",
            vec![call("c1", "echo")],
        )); // 2
        c.push(Message::tool_result("c1", "drain me B also long", false)); // 3
                                                                           // Surviving pair AFTER drain_to (idx 4,5): assistant call c2 + its result.
        c.push(Message::assistant(
            "SURVIVOR keep me",
            vec![call("c2", "echo")],
        )); // 4
        c.push(Message::tool_result(
            "c2",
            "ORIGINAL TOOL OUTPUT THAT IS LONG ENOUGH",
            false,
        )); // 5 ← rewrite target
        c.push(Message::user("TAIL keep me")); // 6
        let floor = c.sacred_floor(); // 2
        let sys_before = c.messages[0].clone();
        let task_before = c.messages[1].clone();
        let survivor_before = c.messages[4].clone();
        let tail_before = c.messages[6].clone();
        let epoch_before = c.cache_epoch;

        // Drain [2,4); insert a summary at floor; rewrites in ORIGINAL index space:
        //   (5, …)        → surviving tool result AFTER drain_to → must apply, translated.
        //   (3, "SKIPPED")→ inside the drained range → must be skipped.
        //   (0, "HACKED") → sacred prefix → must be skipped.
        let plan = CompactionPlan {
            drain_from: 2,
            drain_to: 4,
            summary: Some("s".into()),
            rewrites: vec![
                (5, "[stub]".into()),
                (3, "SHOULD-BE-SKIPPED-DRAINED".into()),
                (0, "HACKED".into()),
            ],
            resume_note: None,
        };
        let report = c.apply_plan(plan, floor);
        assert!(report.committed, "net-smaller combining plan must commit");
        assert_eq!(c.cache_epoch, epoch_before + 1);

        // Post-compaction layout:
        //   [0]=SYS [1]=TASK [2]=summary(synthetic) [3]=SURVIVOR(orig4)
        //   [4]=tool result(orig5, REWRITTEN) [5]=TAIL(orig6)
        // 7 original - 2 drained + 1 summary = 6.
        assert_eq!(
            c.messages.len(),
            6,
            "7 original - 2 drained + 1 summary = 6"
        );
        // Sacred prefix byte-identical (the (0,..) rewrite was skipped).
        assert_eq!(
            c.messages[0], sys_before,
            "system byte-identical, sacred rewrite skipped"
        );
        assert_eq!(c.messages[1], task_before, "task byte-identical");
        // Summary inserted at the floor.
        assert!(c.messages[2].synthetic && c.messages[2].text == "s");
        // SURVIVOR (orig 4) is unchanged — it was NOT the rewrite target.
        assert_eq!(
            c.messages[3], survivor_before,
            "survivor assistant untouched"
        );
        // The CORRECT surviving message (orig 5 → candidate 4) got the rewrite,
        // NOT an off-by-N neighbor.
        assert_eq!(
            c.messages[4].text, "[stub]",
            "rewrite hit the translated index"
        );
        assert_eq!(
            c.messages[4].tool_call_id.as_deref(),
            Some("c2"),
            "and it is the right message"
        );
        // The drained-index rewrite never resurfaced anywhere.
        assert!(
            !c.messages
                .iter()
                .any(|m| m.text == "SHOULD-BE-SKIPPED-DRAINED"),
            "a rewrite of a drained index must be silently skipped"
        );
        // No message got HACKED.
        assert!(
            !c.messages.iter().any(|m| m.text == "HACKED"),
            "sacred-prefix rewrite skipped"
        );
        // Tail preserved.
        assert_eq!(c.messages[5], tail_before, "trailing user preserved");
    }

    // NoCompaction (the neutral default strategy) returns a noop plan.
    #[tokio::test]
    async fn no_compaction_plans_noop() {
        let msgs = vec![Message::system("s"), Message::user("u")];
        let view = CompactionView {
            messages: &msgs,
            trigger: CompactTrigger::Auto { utilization: 0.9 },
            ctx_window: 1000,
            used_tokens: 900,
            utilization: 0.9,
            sacred_floor: 2,
        };
        let plan = NoCompaction.plan(&view).await;
        assert!(plan.is_noop(), "NoCompaction must return a noop plan");
        assert_eq!(plan, CompactionPlan::noop());
    }
}
