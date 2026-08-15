use atomcode_kernel::message::{
    ImageContent, Message as KernelMessage, Role as KernelRole, LEGACY_COLD_SUMMARY_ORIGIN,
    LEGACY_COLD_SUMMARY_PREFIX,
};
use serde::{Deserialize, Serialize};

use atomcode_capabilities::session::manager::{NativeImportCommitOutcome, META_VERSION};
use atomcode_capabilities::session::presentation::PRESENTATION_VERSION;
use atomcode_capabilities::session::{
    anchor_from_legacy_position, DisplayAnchor, ImportInfo, ImportKind, LegacyTurnBoundary,
    PresentationEntry, PresentationFile, PresentationRole, SessionLease, SessionManager,
    SessionMeta, SessionResult, SessionStoreError, StorageOwner, TurnStat,
};
use atomcode_capabilities::session::manager::SessionOrigin;

/// In-memory result of the one legacy → native conversion. S2b owns persistence
/// and commit; keeping this function side-effect free makes conversion testable.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedLegacySession {
    pub snapshot: atomcode_kernel::message::SessionSnapshot,
    pub meta: SessionMeta,
    pub presentation: PresentationFile,
}

/// Core-free read model shared by daemon/TUI catalog consumers. Native-owned
/// sessions come from the strict capabilities aggregate; legacy-only sessions
/// are converted in memory without committing ownership or writing files.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSessionView {
    pub snapshot: atomcode_kernel::message::SessionSnapshot,
    pub meta: SessionMeta,
    pub presentation: PresentationFile,
}

/// Bounded, presentation-only read model for `/resume` selection previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSessionPreview {
    pub session_id: String,
    pub project_bucket: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub excerpt: Vec<String>,
}

/// Sanitized preview failures. Display strings intentionally contain no paths,
/// raw serde diagnostics, or stored session content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSessionPreviewError {
    InvalidLocation,
    Corrupt,
    TooLarge,
    Unavailable,
}

impl std::fmt::Display for CatalogSessionPreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidLocation => "invalid session preview location",
            Self::Corrupt => "session preview data is corrupt",
            Self::TooLarge => "session preview data is too large",
            Self::Unavailable => "session preview is unavailable",
        })
    }
}

impl std::error::Error for CatalogSessionPreviewError {}

/// Exact catalog selection prepared for a runtime resume. The importer and the
/// replacement runtime share `lease`; dropping this value before handing the
/// guard to `CodingRuntime` intentionally abandons the switch.
#[derive(Clone, Debug)]
pub struct PreparedCatalogSessionResume {
    pub project_bucket: String,
    pub view: CatalogSessionView,
    pub lease: SessionLease,
}

impl From<ConvertedLegacySession> for CatalogSessionView {
    fn from(value: ConvertedLegacySession) -> Self {
        Self {
            snapshot: value.snapshot,
            meta: value.meta,
            presentation: value.presentation,
        }
    }
}

impl From<atomcode_capabilities::session::LoadedSession> for CatalogSessionView {
    fn from(value: atomcode_capabilities::session::LoadedSession) -> Self {
        Self {
            snapshot: value.snapshot,
            meta: value.meta,
            presentation: value.presentation,
        }
    }
}

// v3 could destructively delete presentation entries while repairing old
// metadata-only imports. v4 re-audits v3 output and only changes accounting
// metadata; presentation is always preserved byte-for-byte.
pub const IMPORTER_VERSION: u32 = 4;
pub const LEGACY_SCHEMA: &str = "core-session-json";

// ---------------------------------------------------------------------------
// Frozen DTOs — self-contained read model for the retired core session JSON.
// Every serde attribute mirrors the retired core conversation message shape
// exactly so that existing <id>.json files round-trip without deserialization
// loss.
// ---------------------------------------------------------------------------

/// Verbatim copy of core `ToolCall` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Verbatim copy of core `ThinkingBlock` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyThinkingBlock {
    text: String,
    signature: String,
}

/// Verbatim copy of core `MessageContent` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum LegacyContent {
    Text(String),
    AssistantWithToolCalls {
        text: Option<String>,
        tool_calls: Vec<LegacyToolCall>,
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        thinking_blocks: Vec<LegacyThinkingBlock>,
    },
    ToolResult(LegacyToolResult),
    ToolResultRef(LegacyToolResultRef),
    MultiPart {
        text: Option<String>,
        images: Vec<LegacyImagePart>,
    },
}

/// Verbatim copy of core `ToolResult` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyToolResult {
    call_id: String,
    output: String,
    success: bool,
}

/// Verbatim copy of core `ToolResultRef` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyToolResultRef {
    call_id: String,
    hash: String,
    summary: String,
    byte_size: usize,
    success: bool,
}

/// Verbatim copy of core `ImagePart` serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyImagePart {
    media_type: String,
    data: String,
}

/// Frozen message DTO: mirrors core `Message` serde shape verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyMessage {
    role: String,
    content: LegacyContent,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    synthetic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    internal_origin: Option<String>,
}

// ---------------------------------------------------------------------------

/// Frozen reader for the retired core session JSON schema. Keeping this DTO
/// private prevents legacy persistence fields from leaking back into drivers.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySession {
    id: String,
    name: String,
    working_dir: std::path::PathBuf,
    created_at: u64,
    updated_at: u64,
    messages: Vec<LegacyMessage>,
    #[serde(default)]
    display_messages: Vec<LegacyDisplayMessage>,
    #[serde(default)]
    cold_summaries: Vec<String>,
    #[serde(default)]
    user_renamed: bool,
    #[serde(default)]
    ai_named: bool,
    #[serde(default)]
    turn_stats: Vec<LegacyTurnStat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyDisplayMessage {
    after_message: usize,
    message: LegacyMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTurnStat {
    after_message: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_count: Option<usize>,
    tool_call_count: usize,
    duration_ms: u64,
    total_tokens: usize,
    #[serde(default)]
    errored: bool,
    #[serde(default)]
    used_tokens: usize,
    #[serde(default)]
    ctx_window: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    AlreadyNative,
    AdoptedNative,
    ImportedFull,
    ImportedMetadataOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportDiagnostic {
    LegacyChangedAfterCutover,
    RepairedLegacyTurnBoundaries {
        dropped_turn_stats: usize,
    },
    DefaultedLegacyTurnCounts {
        repaired_turn_stats: usize,
    },
    RepairedLegacyTurnStats {
        dropped_turn_stats: usize,
        defaulted_turn_counts: usize,
    },
    RepairedMetadataOnlySidecars {
        repaired_turn_stats: usize,
        removed_presentation_entries: usize,
    },
    MetadataOnlySidecarsUnresolved,
}

fn report_import_diagnostic(session_id: &str, diagnostic: Option<ImportDiagnostic>) {
    match diagnostic {
        Some(ImportDiagnostic::RepairedLegacyTurnBoundaries { dropped_turn_stats }) => {
            tracing::warn!(
                session_id,
                dropped_turn_stats,
                "repaired malformed legacy turn boundaries during import"
            );
        }
        Some(ImportDiagnostic::DefaultedLegacyTurnCounts {
            repaired_turn_stats,
        }) => {
            tracing::warn!(
                session_id,
                repaired_turn_stats,
                "defaulted missing legacy turn_count fields during import"
            );
        }
        Some(ImportDiagnostic::RepairedLegacyTurnStats {
            dropped_turn_stats,
            defaulted_turn_counts,
        }) => {
            tracing::warn!(
                session_id,
                dropped_turn_stats,
                "repaired malformed legacy turn boundaries during import"
            );
            tracing::warn!(
                session_id,
                repaired_turn_stats = defaulted_turn_counts,
                "defaulted missing legacy turn_count fields during import"
            );
        }
        Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
            repaired_turn_stats,
            removed_presentation_entries,
        }) => {
            tracing::warn!(
                session_id,
                repaired_turn_stats,
                removed_presentation_entries,
                "repaired old metadata-only session sidecars"
            );
        }
        Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved) => {
            tracing::warn!(
                session_id,
                "could not identify old metadata-only sidecars; preserving native data"
            );
        }
        Some(ImportDiagnostic::LegacyChangedAfterCutover) => {
            tracing::warn!(
                session_id,
                "legacy source changed after native cutover; preserving native data"
            );
        }
        None => {}
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportOutcome {
    pub status: ImportStatus,
    pub diagnostic: Option<ImportDiagnostic>,
    pub snapshot: atomcode_kernel::message::SessionSnapshot,
    pub meta: SessionMeta,
    pub presentation: PresentationFile,
}

fn legacy_image_to_kernel(image: &LegacyImagePart) -> ImageContent {
    ImageContent {
        media_type: image.media_type.clone(),
        data: image.data.clone(),
    }
}

fn legacy_role_to_kernel(role: &str) -> KernelRole {
    match role {
        "System" => KernelRole::System,
        "User" => KernelRole::User,
        "Assistant" => KernelRole::Assistant,
        "Tool" => KernelRole::Tool,
        // Unrecognised roles fall back to User; validate_tool_pairing will
        // catch structural issues in the message sequence.
        _ => KernelRole::User,
    }
}

/// Convert a frozen legacy DTO message to a kernel message without going
/// through core::conversation types.
fn legacy_message_to_kernel(message: &LegacyMessage) -> KernelMessage {
    let mut converted = match &message.content {
        LegacyContent::Text(text) => {
            let mut converted = KernelMessage::user(text.clone());
            converted.role = legacy_role_to_kernel(&message.role);
            converted
        }
        LegacyContent::AssistantWithToolCalls {
            text,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        } => {
            let mut converted = KernelMessage::assistant(
                text.clone().unwrap_or_default(),
                tool_calls
                    .iter()
                    .map(|call| atomcode_kernel::tool::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                    .collect(),
            );
            converted.reasoning = reasoning_content.clone();
            converted.reasoning_blocks = thinking_blocks
                .iter()
                .map(|block| atomcode_kernel::message::ReasoningBlock {
                    text: block.text.clone(),
                    opaque: Some(block.signature.clone()),
                    // Legacy files did not persist provider identity. The signature
                    // is lossless, but attributing it to Anthropic would be a guess.
                    provider: None,
                })
                .collect();
            converted
        }
        LegacyContent::ToolResult(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.output.clone(),
            !result.success,
        ),
        LegacyContent::ToolResultRef(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.summary.clone(),
            !result.success,
        ),
        LegacyContent::MultiPart { text, images } => KernelMessage::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(legacy_image_to_kernel).collect(),
        ),
    };
    converted.synthetic = message.synthetic;
    converted.internal_origin = message.internal_origin.clone();
    converted
}

struct NormalizedLegacyTurns {
    boundaries: Vec<LegacyTurnBoundary>,
    turn_stats: Vec<TurnStat>,
    dropped_turn_stats: usize,
    defaulted_turn_counts: usize,
}

fn normalize_legacy_turns(session: &LegacySession) -> anyhow::Result<NormalizedLegacyTurns> {
    let mut boundaries = Vec::with_capacity(session.turn_stats.len());
    let mut turn_stats = Vec::with_capacity(session.turn_stats.len());
    let mut previous_after = 0usize;
    let mut dropped_turn_stats = 0usize;
    let mut defaulted_turn_counts = 0usize;

    for stat in &session.turn_stats {
        if stat.after_message > session.messages.len()
            || stat.after_message == 0
            || stat.after_message <= previous_after
        {
            dropped_turn_stats += 1;
            continue;
        }
        if stat.turn_count.is_none() {
            defaulted_turn_counts += 1;
        }

        let turn_id = u64::try_from(turn_stats.len())?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("legacy turn id overflow"))?;
        boundaries.push(LegacyTurnBoundary {
            after_message: stat.after_message,
            turn_id,
        });
        turn_stats.push(TurnStat {
            after_message: stat.after_message,
            position_valid: true,
            turn_id,
            round_count: checked_u32(stat.turn_count.unwrap_or_default(), "turn_count")?,
            tool_call_count: checked_u32(stat.tool_call_count, "tool_call_count")?,
            duration_ms: stat.duration_ms,
            total_tokens: checked_u32(stat.total_tokens, "total_tokens")?,
            errored: stat.errored,
            used_tokens: checked_u32(stat.used_tokens, "used_tokens")?,
            ctx_window: checked_u32(stat.ctx_window, "ctx_window")?,
            model_usage: Vec::new(),
        });
        previous_after = stat.after_message;
    }

    // Keep the native anchor invariant strict; only the frozen legacy reader is
    // allowed to repair malformed historical offsets.
    anchor_from_legacy_position(0, &boundaries)?;
    Ok(NormalizedLegacyTurns {
        boundaries,
        turn_stats,
        dropped_turn_stats,
        defaulted_turn_counts,
    })
}

fn anchor_from_normalized_legacy_position(
    after_message: usize,
    boundaries: &[LegacyTurnBoundary],
    repaired_turn_boundaries: bool,
) -> SessionResult<DisplayAnchor> {
    if !repaired_turn_boundaries {
        return anchor_from_legacy_position(after_message, boundaries);
    }
    let Some(last) = boundaries.last() else {
        return Ok(DisplayAnchor::AtStart);
    };
    if after_message > last.after_message {
        return Ok(DisplayAnchor::AfterTurn {
            turn_id: last.turn_id,
        });
    }
    anchor_from_legacy_position(after_message, boundaries)
}

/// Convert the complete legacy session DTO without reading or writing files.
/// Persistence, ownership and importer recovery are deliberately S2b concerns.
fn convert_legacy_session(session: &LegacySession) -> anyhow::Result<ConvertedLegacySession> {
    let (converted, diagnostic) = convert_legacy_session_with_diagnostic(session)?;
    report_import_diagnostic(&session.id, diagnostic);
    Ok(converted)
}

fn convert_legacy_session_with_diagnostic(
    session: &LegacySession,
) -> anyhow::Result<(ConvertedLegacySession, Option<ImportDiagnostic>)> {
    let id = session.id.clone();
    let working_dir = session
        .working_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("legacy working directory is not valid UTF-8"))?
        .to_string();
    let created_at = seconds_to_millis(session.created_at, "created_at")?;
    let updated_at = seconds_to_millis(session.updated_at, "updated_at")?;

    let normalized_turns = normalize_legacy_turns(session)?;
    let boundaries = &normalized_turns.boundaries;

    let presentation = PresentationFile {
        v: PRESENTATION_VERSION,
        entries: session
            .display_messages
            .iter()
            .map(|display| {
                if display.after_message > session.messages.len() {
                    anyhow::bail!("legacy presentation position is outside the message history")
                }
                let role = match display.message.role.as_str() {
                    "User" => PresentationRole::User,
                    "Assistant" => PresentationRole::Assistant,
                    role => {
                        anyhow::bail!(
                            "legacy presentation role {role:?} is not supported by schema v1"
                        )
                    }
                };
                let LegacyContent::Text(text) = &display.message.content else {
                    anyhow::bail!("legacy presentation content is not plain text")
                };
                Ok(PresentationEntry {
                    anchor: anchor_from_normalized_legacy_position(
                        display.after_message,
                        boundaries,
                        normalized_turns.dropped_turn_stats > 0,
                    )?,
                    role,
                    text: text.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
    };

    // Build the kernel snapshot from frozen DTOs directly, without going through
    // core::conversation types. Cold summaries are prepended as synthetic messages
    // (mirrors snapshot_to_kernel's historical behaviour exactly).
    let mut snapshot_messages =
        Vec::with_capacity(session.cold_summaries.len() + session.messages.len());
    for summary in &session.cold_summaries {
        let mut msg = KernelMessage::user(format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}"));
        msg.synthetic = true;
        msg.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        snapshot_messages.push(msg);
    }
    snapshot_messages.extend(session.messages.iter().map(legacy_message_to_kernel));
    let mut snapshot = atomcode_kernel::message::SessionSnapshot::new(snapshot_messages);
    validate_tool_pairing(&snapshot.messages)?;
    // Imported presentation anchors consume stable historical turn ids. Seed the
    // kernel above them so the first resumed turn cannot reuse an imported id.
    snapshot.turn_counter = snapshot
        .turn_counter
        .max(boundaries.last().map_or(0, |boundary| boundary.turn_id));

    let mut meta = SessionMeta {
        v: META_VERSION,
        id,
        name: session.name.clone(),
        user_renamed: session.user_renamed,
        ai_named: session.ai_named,
        owner: StorageOwner::Legacy,
        import_info: None,
        fork_info: None,
        working_dir,
        created_at,
        updated_at,
        turn_count: checked_u32(normalized_turns.turn_stats.len(), "turn_stats")?,
        message_count: checked_u32(session.messages.len(), "messages")?,
        turn_stats: normalized_turns.turn_stats,
        detached_model_usage: Vec::new(),
        detached_unattributed_tokens: 0,
        origin: SessionOrigin::Manual,
    };
    meta.auto_name_from_messages(&snapshot.messages);

    let diagnostic =
        if normalized_turns.dropped_turn_stats > 0 && normalized_turns.defaulted_turn_counts > 0 {
            Some(ImportDiagnostic::RepairedLegacyTurnStats {
                dropped_turn_stats: normalized_turns.dropped_turn_stats,
                defaulted_turn_counts: normalized_turns.defaulted_turn_counts,
            })
        } else if normalized_turns.dropped_turn_stats > 0 {
            Some(ImportDiagnostic::RepairedLegacyTurnBoundaries {
                dropped_turn_stats: normalized_turns.dropped_turn_stats,
            })
        } else if normalized_turns.defaulted_turn_counts > 0 {
            Some(ImportDiagnostic::DefaultedLegacyTurnCounts {
                repaired_turn_stats: normalized_turns.defaulted_turn_counts,
            })
        } else {
            None
        };

    Ok((
        ConvertedLegacySession {
            snapshot,
            meta,
            presentation,
        },
        diagnostic,
    ))
}

const MAX_IMPORT_CAS_RETRIES: usize = 8;

fn adopt_unconfirmed_native(
    manager: &SessionManager,
    lease: &SessionLease,
    mut expected_meta: SessionMeta,
    mut expected_snapshot: Option<atomcode_kernel::message::SessionSnapshot>,
    mut expected_presentation: Option<PresentationFile>,
) -> anyhow::Result<ImportOutcome> {
    for _ in 0..MAX_IMPORT_CAS_RETRIES {
        if expected_meta.owner == StorageOwner::Native {
            let snapshot = expected_snapshot.ok_or_else(|| {
                anyhow::anyhow!(
                    "owner=native session {:?} is missing its snapshot",
                    lease.id()
                )
            })?;
            let presentation = expected_presentation.ok_or_else(|| {
                anyhow::anyhow!(
                    "owner=native session {:?} is missing presentation",
                    lease.id()
                )
            })?;
            return Ok(ImportOutcome {
                status: ImportStatus::AlreadyNative,
                diagnostic: None,
                snapshot,
                meta: expected_meta,
                presentation,
            });
        }
        if expected_meta.owner != StorageOwner::Unconfirmed {
            anyhow::bail!(
                "session {:?} changed owner to {:?} during native adoption",
                lease.id(),
                expected_meta.owner
            )
        }
        let snapshot = expected_snapshot.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "session {:?} lost its snapshot during native adoption",
                lease.id()
            )
        })?;
        let presentation = expected_presentation.clone().unwrap_or_default();
        let mut committed_meta = expected_meta.clone();
        committed_meta.auto_name_from_messages(&snapshot.messages);
        committed_meta.owner = StorageOwner::Native;
        committed_meta.import_info = None;
        let write_presentation = expected_presentation.is_none();

        match manager.commit_native_import_if_unchanged(
            lease,
            &expected_meta,
            expected_snapshot.as_ref(),
            expected_presentation.as_ref(),
            None,
            write_presentation.then_some(&presentation),
            &committed_meta,
        )? {
            NativeImportCommitOutcome::Committed(meta) => {
                return Ok(ImportOutcome {
                    status: ImportStatus::AdoptedNative,
                    diagnostic: None,
                    snapshot: snapshot.clone(),
                    meta,
                    presentation,
                })
            }
            NativeImportCommitOutcome::Conflict {
                meta,
                snapshot,
                presentation,
            } => {
                expected_meta = meta;
                expected_snapshot = snapshot;
                expected_presentation = presentation;
            }
        }
    }
    anyhow::bail!(
        "session {:?} changed repeatedly during native adoption; retry",
        lease.id()
    )
}

fn import_metadata_only_with_cas(
    manager: &SessionManager,
    lease: &SessionLease,
    legacy_bytes: &[u8],
    converted: &ConvertedLegacySession,
    diagnostic: Option<ImportDiagnostic>,
    mut expected_meta: SessionMeta,
    mut expected_snapshot: Option<atomcode_kernel::message::SessionSnapshot>,
    mut expected_presentation: Option<PresentationFile>,
) -> anyhow::Result<ImportOutcome> {
    for _ in 0..MAX_IMPORT_CAS_RETRIES {
        if expected_meta.owner != StorageOwner::Unconfirmed {
            anyhow::bail!(
                "session {:?} changed owner to {:?} during metadata-only import",
                lease.id(),
                expected_meta.owner
            )
        }
        let original_snapshot = expected_snapshot.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "session {:?} lost its snapshot during metadata-only import",
                lease.id()
            )
        })?;
        let mut rebased = converted.clone();
        rebase_converted_turn_ids(&mut rebased, original_snapshot.turn_counter)?;
        let mut snapshot = original_snapshot.clone();
        let mut meta = expected_meta.clone();
        meta.auto_name_from_messages(&snapshot.messages);
        if meta.turn_stats.is_empty() {
            meta.turn_stats = rebased.meta.turn_stats;
            for stat in &mut meta.turn_stats {
                stat.position_valid = false;
            }
        } else if meta.turn_stats.iter().any(|stat| stat.turn_id == 0) {
            let native_suffix: Vec<_> = meta
                .turn_stats
                .into_iter()
                .filter(|stat| stat.turn_id != 0)
                .collect();
            meta.turn_stats = rebased.meta.turn_stats;
            for stat in &mut meta.turn_stats {
                stat.position_valid = false;
            }
            meta.turn_stats.extend(native_suffix);
        }
        meta.turn_count = u32::try_from(meta.turn_stats.len())
            .map_err(|_| anyhow::anyhow!("native turn stat count exceeds u32"))?;
        snapshot.turn_counter = snapshot.turn_counter.max(
            meta.turn_stats
                .iter()
                .map(|stat| stat.turn_id)
                .max()
                .unwrap_or(0),
        );
        meta.message_count = u32::try_from(snapshot.messages.len())
            .map_err(|_| anyhow::anyhow!("native snapshot message count exceeds u32"))?;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: IMPORTER_VERSION,
            kind: ImportKind::MetadataOnly,
        });
        let presentation = expected_presentation.clone().unwrap_or_default();
        let write_snapshot = snapshot != *original_snapshot;
        let write_presentation = expected_presentation.is_none();

        match manager.commit_native_import_if_unchanged(
            lease,
            &expected_meta,
            expected_snapshot.as_ref(),
            expected_presentation.as_ref(),
            write_snapshot.then_some(&snapshot),
            write_presentation.then_some(&presentation),
            &meta,
        )? {
            NativeImportCommitOutcome::Committed(meta) => {
                report_import_diagnostic(lease.id(), diagnostic);
                return Ok(ImportOutcome {
                    status: ImportStatus::ImportedMetadataOnly,
                    diagnostic,
                    snapshot,
                    meta,
                    presentation,
                });
            }
            NativeImportCommitOutcome::Conflict {
                meta,
                snapshot,
                presentation,
            } => {
                expected_meta = meta;
                expected_snapshot = snapshot;
                expected_presentation = presentation;
            }
        }
    }
    anyhow::bail!(
        "session {:?} changed repeatedly during metadata-only import; retry",
        lease.id()
    )
}

/// Resolve a session's storage state and, when needed, publish native ownership.
/// The caller must hold the exact bucket/session lease for the whole operation.
pub fn converge_session(
    manager: &SessionManager,
    lease: &SessionLease,
) -> anyhow::Result<ImportOutcome> {
    converge_session_with_retries(manager, lease, MAX_IMPORT_CAS_RETRIES)
}

fn converge_session_with_retries(
    manager: &SessionManager,
    lease: &SessionLease,
    remaining_retries: usize,
) -> anyhow::Result<ImportOutcome> {
    manager.validate_active_lease(lease)?;
    let id = lease.id();
    let existing_meta = optional_store(manager.read_meta(id))?;
    let legacy_bytes = optional_store(manager.read_legacy_bytes(id))?;

    if existing_meta.as_ref().map(|meta| &meta.owner) == Some(&StorageOwner::Native) {
        let loaded = manager.load_native_session(id)?;
        let mut meta = loaded.meta;
        let mut snapshot = loaded.snapshot;
        let mut presentation = loaded.presentation;
        let expected_meta = meta.clone();
        let expected_snapshot = snapshot.clone();
        let expected_presentation = presentation.clone();
        if let Some(bytes) = legacy_bytes.as_deref() {
            let recoverable_empty_import = meta.message_count == 0
                && snapshot.messages.is_empty()
                && presentation.entries.is_empty()
                && meta.import_info.as_ref().is_some_and(|info| {
                    info.kind == ImportKind::MetadataOnly && info.source_sha256 == sha256_hex(bytes)
                });
            if recoverable_empty_import {
                let legacy: LegacySession = serde_json::from_slice(bytes)
                    .map_err(|error| anyhow::anyhow!("invalid legacy session {id:?}: {error}"))?;
                if legacy.id != id {
                    anyhow::bail!(
                        "legacy filename id {id:?} does not match stored id {:?}",
                        legacy.id
                    )
                }
                let (converted, diagnostic) = convert_legacy_session_with_diagnostic(&legacy)?;
                if !converted.snapshot.messages.is_empty() {
                    let mut recovered_meta = converted.meta;
                    recovered_meta.auto_name_from_messages(&converted.snapshot.messages);
                    if meta.user_renamed || meta.ai_named {
                        recovered_meta.name = meta.name.clone();
                        recovered_meta.user_renamed = meta.user_renamed;
                        recovered_meta.ai_named = meta.ai_named;
                    }
                    recovered_meta.updated_at = recovered_meta.updated_at.max(meta.updated_at);
                    recovered_meta.message_count = u32::try_from(converted.snapshot.messages.len())
                        .map_err(|_| {
                            anyhow::anyhow!("native snapshot message count exceeds u32")
                        })?;
                    recovered_meta.owner = StorageOwner::Native;
                    recovered_meta.import_info = Some(ImportInfo {
                        legacy_schema: LEGACY_SCHEMA.into(),
                        source_sha256: sha256_hex(bytes),
                        importer_version: IMPORTER_VERSION,
                        kind: ImportKind::Full,
                    });
                    match manager.recover_empty_metadata_only_import_if_unchanged(
                        lease,
                        &expected_meta,
                        &expected_snapshot,
                        &expected_presentation,
                        &converted.snapshot,
                        &converted.presentation,
                        &recovered_meta,
                    )? {
                        NativeImportCommitOutcome::Committed(meta) => {
                            report_import_diagnostic(id, diagnostic);
                            return Ok(ImportOutcome {
                                status: ImportStatus::ImportedFull,
                                diagnostic,
                                snapshot: converted.snapshot,
                                meta,
                                presentation: converted.presentation,
                            });
                        }
                        NativeImportCommitOutcome::Conflict { .. } => {
                            if remaining_retries == 0 {
                                anyhow::bail!(
                                    "session {id:?} changed repeatedly during empty import recovery; retry"
                                )
                            }
                            return converge_session_with_retries(
                                manager,
                                lease,
                                remaining_retries - 1,
                            );
                        }
                    }
                }
            }
        }
        let mut diagnostic = match (legacy_bytes.as_deref(), meta.import_info.as_ref()) {
            (Some(bytes), Some(info)) if sha256_hex(bytes) != info.source_sha256 => {
                Some(ImportDiagnostic::LegacyChangedAfterCutover)
            }
            _ => None,
        };
        if diagnostic.is_none() {
            if let Some(bytes) = legacy_bytes.as_deref() {
                diagnostic = match repair_metadata_only_sidecars(
                    bytes,
                    snapshot.messages.len(),
                    &mut meta,
                    &mut presentation,
                ) {
                    Ok(diagnostic) => diagnostic,
                    Err(error) => {
                        tracing::warn!(
                            session_id = id,
                            error = %error,
                            "could not inspect old metadata-only sidecars; preserving native data"
                        );
                        Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
                    }
                };
                if matches!(
                    diagnostic,
                    Some(ImportDiagnostic::RepairedMetadataOnlySidecars { .. })
                ) && !manager.commit_native_sidecar_repair_if_unchanged(
                    lease,
                    &expected_meta,
                    &expected_snapshot,
                    &expected_presentation,
                    &meta,
                )? {
                    let current = manager.load_native_session(id)?;
                    meta = current.meta;
                    snapshot = current.snapshot;
                    presentation = current.presentation;
                    diagnostic = Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved);
                }
            } else if meta.import_info.as_ref().is_some_and(|info| {
                info.kind == ImportKind::MetadataOnly && info.importer_version < IMPORTER_VERSION
            }) {
                diagnostic = Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved);
            }
        }
        report_import_diagnostic(id, diagnostic);
        return Ok(ImportOutcome {
            status: ImportStatus::AlreadyNative,
            diagnostic,
            snapshot,
            meta,
            presentation,
        });
    }

    let force_legacy =
        existing_meta.as_ref().map(|meta| &meta.owner) == Some(&StorageOwner::Legacy);
    let existing_snapshot = if force_legacy {
        optional_replaceable_legacy_sidecar(manager.load_snapshot(id))?
    } else {
        optional_store(manager.load_snapshot(id))?
    };
    let existing_presentation = if force_legacy {
        optional_replaceable_legacy_sidecar(manager.read_presentation(id))?
    } else {
        optional_store(manager.read_presentation(id))?
    };

    if legacy_bytes.is_some() && existing_snapshot.is_some() && existing_meta.is_none() {
        anyhow::bail!(
            "session {id:?} has an ambiguous native snapshot without storage ownership metadata; refusing automatic legacy cutover"
        )
    }

    let Some(legacy_bytes) = legacy_bytes else {
        return match (existing_meta, existing_snapshot) {
            (Some(meta), Some(snapshot)) if meta.owner == StorageOwner::Unconfirmed => {
                adopt_unconfirmed_native(
                    manager,
                    lease,
                    meta,
                    Some(snapshot),
                    existing_presentation,
                )
            }
            (None, None) => Err(anyhow::anyhow!("session {id:?} was not found")),
            _ => Err(anyhow::anyhow!(
                "session {id:?} has incomplete native storage and no legacy source"
            )),
        };
    };

    let legacy: LegacySession = serde_json::from_slice(&legacy_bytes)
        .map_err(|error| anyhow::anyhow!("invalid legacy session {id:?}: {error}"))?;
    if legacy.id != id {
        anyhow::bail!(
            "legacy filename id {id:?} does not match stored id {:?}",
            legacy.id
        )
    }
    let (converted, diagnostic) = convert_legacy_session_with_diagnostic(&legacy)?;
    let empty_unconfirmed_stub = existing_meta
        .as_ref()
        .is_some_and(|meta| meta.owner == StorageOwner::Unconfirmed && meta.message_count == 0)
        && existing_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.messages.is_empty())
        && existing_presentation
            .as_ref()
            .is_some_and(|presentation| presentation.entries.is_empty());
    let preserve_native_snapshot =
        existing_snapshot.is_some() && !force_legacy && !empty_unconfirmed_stub;
    if preserve_native_snapshot {
        let meta = existing_meta.ok_or_else(|| {
            anyhow::anyhow!("session {id:?} has a native snapshot without ownership metadata")
        })?;
        return import_metadata_only_with_cas(
            manager,
            lease,
            &legacy_bytes,
            &converted,
            diagnostic,
            meta,
            existing_snapshot,
            existing_presentation,
        );
    }

    let mut snapshot = converted.snapshot;
    let mut meta = converted.meta;
    meta.auto_name_from_messages(&snapshot.messages);
    if let Some(existing) = existing_meta.as_ref() {
        if existing.user_renamed || existing.ai_named {
            meta.name = existing.name.clone();
            meta.user_renamed = existing.user_renamed;
            meta.ai_named = existing.ai_named;
        }
        meta.updated_at = meta.updated_at.max(existing.updated_at);
    }
    snapshot.turn_counter = snapshot.turn_counter.max(
        meta.turn_stats
            .iter()
            .map(|stat| stat.turn_id)
            .max()
            .unwrap_or(0),
    );
    meta.message_count = u32::try_from(snapshot.messages.len())
        .map_err(|_| anyhow::anyhow!("native snapshot message count exceeds u32"))?;
    meta.owner = StorageOwner::Native;
    meta.import_info = Some(ImportInfo {
        legacy_schema: LEGACY_SCHEMA.into(),
        source_sha256: sha256_hex(&legacy_bytes),
        importer_version: IMPORTER_VERSION,
        kind: ImportKind::Full,
    });

    let presentation = converted.presentation;
    let mut intent = meta.clone();
    intent.owner = StorageOwner::Legacy;
    intent.import_info = None;
    if force_legacy {
        manager.begin_legacy_import(lease, &intent)?;
    } else if !manager.begin_legacy_import_if_unchanged(
        lease,
        existing_meta.as_ref(),
        existing_snapshot.as_ref(),
        existing_presentation.as_ref(),
        &intent,
    )? {
        if remaining_retries == 0 {
            anyhow::bail!("session {id:?} changed repeatedly during full legacy import; retry")
        }
        return converge_session_with_retries(manager, lease, remaining_retries - 1);
    }
    manager.commit_native_import(lease, Some(&snapshot), Some(&presentation), &meta)?;
    report_import_diagnostic(id, diagnostic);

    Ok(ImportOutcome {
        status: ImportStatus::ImportedFull,
        diagnostic,
        snapshot,
        meta,
        presentation,
    })
}

pub fn catalog_for_project(
    working_dir: &std::path::Path,
) -> anyhow::Result<Vec<atomcode_capabilities::session::CatalogEntry>> {
    catalog_for_project_in_root(&SessionManager::sessions_root(), working_dir)
}

fn catalog_for_project_in_root(
    sessions_root: &std::path::Path,
    working_dir: &std::path::Path,
) -> anyhow::Result<Vec<atomcode_capabilities::session::CatalogEntry>> {
    let bucket = SessionManager::project_hash(working_dir);
    let scan = SessionManager::scan_catalog(sessions_root);
    report_catalog_diagnostics(&scan.diagnostics);
    let mut entries: Vec<_> = scan
        .entries
        .into_iter()
        .filter(|entry| {
            entry.project_bucket == bucket
                || working_dirs_equivalent(&entry.working_dir, working_dir)
        })
        .collect();
    SessionManager::collapse_fork_lineages(&mut entries);
    repair_catalog_names_for_display_in_root(sessions_root, &mut entries);
    Ok(entries)
}

/// Hydrate only placeholder names for catalog display. Native repairs are
/// persisted by the strict aggregate loader; legacy-only views stay read-only.
/// A damaged entry keeps its scanned name and never hides healthy sessions.
pub(crate) fn repair_catalog_names_for_display_in_root(
    sessions_root: &std::path::Path,
    entries: &mut [atomcode_capabilities::session::CatalogEntry],
) {
    for entry in entries {
        if !SessionMeta::name_needs_fallback(&entry.name, &entry.id) {
            continue;
        }
        match load_catalog_session_view_in_root(sessions_root, entry) {
            Ok(view) => entry.name = view.meta.name,
            Err(error) => tracing::warn!(
                session_id = %entry.id,
                error = %error,
                "session placeholder name repair failed; keeping catalog entry"
            ),
        }
    }
}

fn working_dirs_equivalent(left: &std::path::Path, right: &std::path::Path) -> bool {
    atomcode_capabilities::pathnorm::path_case_key(left)
        == atomcode_capabilities::pathnorm::path_case_key(right)
}

pub fn load_catalog_session_view(
    entry: &atomcode_capabilities::session::CatalogEntry,
) -> anyhow::Result<CatalogSessionView> {
    load_catalog_session_view_in_root(&SessionManager::sessions_root(), entry)
}

pub fn load_catalog_session_view_in_project(
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<Option<CatalogSessionView>> {
    load_catalog_session_view_in_project_root(&SessionManager::sessions_root(), project_bucket, id)
}

pub fn preview_catalog_session_in_project(
    project_bucket: &str,
    id: &str,
) -> Result<Option<CatalogSessionPreview>, CatalogSessionPreviewError> {
    preview_catalog_session_in_project_root(&SessionManager::sessions_root(), project_bucket, id)
}

/// How many recent messages the preview excerpt keeps.
const PREVIEW_EXCERPT_MESSAGES: usize = 6;

/// The most recent `<= PREVIEW_EXCERPT_MESSAGES` messages, each collapsed to a
/// SINGLE sanitized line (its first non-empty line). One line per MESSAGE — not
/// per source line — so a multi-line assistant reply can't fill the whole preview
/// and drop the preceding user prompt (the most useful "which session is this"
/// signal). Messages that sanitize to nothing don't consume a slot.
fn preview_excerpt<'a>(message_texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut excerpt: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(PREVIEW_EXCERPT_MESSAGES);
    for text in message_texts {
        let Some(line) = text
            .lines()
            .map(sanitize_preview_line)
            .find(|line| !line.is_empty())
        else {
            continue;
        };
        if excerpt.len() == PREVIEW_EXCERPT_MESSAGES {
            excerpt.pop_front();
        }
        excerpt.push_back(line);
    }
    excerpt.into()
}

/// Provider/model of the most recent turn that recorded model usage.
fn provider_model_from_meta(meta: &SessionMeta) -> (Option<String>, Option<String>) {
    let latest_usage = meta
        .turn_stats
        .iter()
        .rev()
        .find(|turn| !turn.model_usage.is_empty())
        .and_then(|turn| turn.model_usage.last());
    let provider_id = latest_usage
        .map(|usage| sanitize_preview_line(&usage.provider_id))
        .filter(|value| !value.is_empty());
    let model_id = latest_usage
        .map(|usage| sanitize_preview_line(&usage.model_id))
        .filter(|value| !value.is_empty());
    (provider_id, model_id)
}

pub(crate) fn preview_catalog_session_in_project_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
) -> Result<Option<CatalogSessionPreview>, CatalogSessionPreviewError> {
    validate_project_bucket(project_bucket)
        .map_err(|_| CatalogSessionPreviewError::InvalidLocation)?;
    let manager = SessionManager::with_root(sessions_root.join(project_bucket));

    // Read-only across EVERY on-disk state (never acquires a lease / ownership),
    // so a mere /resume hover cannot converge or mutate a session — yet the
    // preview must work for the whole backlog, not only sessions already resumed
    // into an owner=Native aggregate with a presentation sidecar.
    let (provider_id, model_id, excerpt) = match manager.read_meta(id) {
        Ok(meta) => {
            let (provider_id, model_id) = provider_model_from_meta(&meta);
            let excerpt = match manager.read_presentation(id) {
                // owner=Native: the display presentation is authoritative.
                Ok(presentation) => {
                    preview_excerpt(presentation.entries.iter().map(|entry| entry.text.as_str()))
                }
                // Pre-owner (Unconfirmed) native: no presentation sidecar yet, so
                // derive read-only from the snapshot's user/assistant messages
                // rather than reporting a healthy session as unavailable/corrupt.
                Err(SessionStoreError::NotFound { .. }) => match manager.load_snapshot(id) {
                    Ok(snapshot) => preview_excerpt(
                        snapshot
                            .messages
                            .iter()
                            .filter(|message| {
                                matches!(
                                    message.role,
                                    atomcode_kernel::message::Role::User
                                        | atomcode_kernel::message::Role::Assistant
                                )
                            })
                            .map(|message| message.text.as_str()),
                    ),
                    Err(SessionStoreError::NotFound { .. }) => return Ok(None),
                    Err(error) => return Err(classify_preview_error(&error)),
                },
                Err(error) => return Err(classify_preview_error(&error)),
            };
            (provider_id, model_id, excerpt)
        }
        // Legacy-only: no native meta. Read + convert in memory (no persist, no
        // ownership) purely to project the preview.
        Err(SessionStoreError::NotFound { .. }) => {
            let bytes = match manager.read_legacy_bytes(id) {
                Ok(bytes) => bytes,
                Err(SessionStoreError::NotFound { .. }) => return Ok(None),
                Err(error) => return Err(classify_preview_error(&error)),
            };
            let legacy: LegacySession =
                serde_json::from_slice(&bytes).map_err(|_| CatalogSessionPreviewError::Corrupt)?;
            let converted =
                convert_legacy_session(&legacy).map_err(|_| CatalogSessionPreviewError::Corrupt)?;
            let (provider_id, model_id) = provider_model_from_meta(&converted.meta);
            let excerpt = preview_excerpt(
                converted
                    .presentation
                    .entries
                    .iter()
                    .map(|entry| entry.text.as_str()),
            );
            (provider_id, model_id, excerpt)
        }
        Err(error) => return Err(classify_preview_error(&error)),
    };

    Ok(Some(CatalogSessionPreview {
        session_id: id.to_string(),
        project_bucket: project_bucket.to_string(),
        provider_id,
        model_id,
        excerpt,
    }))
}

fn classify_preview_error(error: &SessionStoreError) -> CatalogSessionPreviewError {
    match error {
        SessionStoreError::InvalidId { .. } | SessionStoreError::UnsafeFile { .. } => {
            CatalogSessionPreviewError::InvalidLocation
        }
        SessionStoreError::TooLarge { .. } => CatalogSessionPreviewError::TooLarge,
        SessionStoreError::FutureSchema { .. } | SessionStoreError::Corrupt { .. } => {
            CatalogSessionPreviewError::Corrupt
        }
        SessionStoreError::NotFound { .. }
        | SessionStoreError::SessionInUse { .. }
        | SessionStoreError::AmbiguousId { .. }
        | SessionStoreError::LeaseMismatch { .. }
        | SessionStoreError::OwnershipConflict { .. }
        | SessionStoreError::Io { .. }
        | SessionStoreError::UncertainCommit { .. } => CatalogSessionPreviewError::Unavailable,
    }
}

fn sanitize_preview_line(line: &str) -> String {
    #[derive(Clone, Copy)]
    enum Escape {
        Text,
        Start,
        Csi,
        Osc,
        OscEscape,
    }

    let mut state = Escape::Text;
    let mut clean = String::with_capacity(line.len().min(512));
    let mut visible = 0usize;
    for character in line.chars() {
        state = match state {
            Escape::Text if character == '\u{1b}' => Escape::Start,
            Escape::Text => {
                if character == '\t' && visible < 512 {
                    clean.push(' ');
                    visible += 1;
                } else if !character.is_control() && visible < 512 {
                    clean.push(character);
                    visible += 1;
                }
                Escape::Text
            }
            Escape::Start if character == '[' => Escape::Csi,
            Escape::Start if character == ']' => Escape::Osc,
            Escape::Start => Escape::Text,
            Escape::Csi if ('@'..='~').contains(&character) => Escape::Text,
            Escape::Csi => Escape::Csi,
            Escape::Osc if character == '\u{7}' => Escape::Text,
            Escape::Osc if character == '\u{1b}' => Escape::OscEscape,
            Escape::Osc => Escape::Osc,
            Escape::OscEscape if character == '\\' => Escape::Text,
            Escape::OscEscape => Escape::Osc,
        };
    }
    clean.trim().to_string()
}

/// Resolve one exact catalog location, converge it to native ownership under an
/// exclusive lease, and return that same guard for transfer into CodingRuntime.
pub fn prepare_catalog_session_resume_in_project(
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<Option<PreparedCatalogSessionResume>> {
    prepare_catalog_session_resume_in_project_root(
        &SessionManager::sessions_root(),
        project_bucket,
        id,
    )
}

/// Resolve one catalog location across any project bucket in the catalog,
/// converge it to native ownership under an exclusive lease, and return that same guard.
pub fn prepare_catalog_session_resume_any_project(
    id: &str,
) -> anyhow::Result<Option<PreparedCatalogSessionResume>> {
    prepare_catalog_session_resume_any_project_in_root(&SessionManager::sessions_root(), id)
}

pub(crate) fn prepare_catalog_session_resume_any_project_in_root(
    sessions_root: &std::path::Path,
    id: &str,
) -> anyhow::Result<Option<PreparedCatalogSessionResume>> {
    let scan = SessionManager::scan_catalog(sessions_root);
    report_catalog_diagnostics(&scan.diagnostics);
    let Some(entry) = scan.entries.iter().find(|entry| entry.id == id) else {
        reject_matching_catalog_diagnostic(&scan.diagnostics, id)?;
        return Ok(None);
    };
    let manager = SessionManager::with_root(sessions_root.join(&entry.project_bucket));
    let lease = manager.acquire_lease(id)?;
    let outcome = converge_session(&manager, &lease)?;
    Ok(Some(PreparedCatalogSessionResume {
        project_bucket: entry.project_bucket.clone(),
        view: CatalogSessionView {
            snapshot: outcome.snapshot,
            meta: outcome.meta,
            presentation: outcome.presentation,
        },
        lease,
    }))
}

pub(crate) fn prepare_catalog_session_resume_in_project_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<Option<PreparedCatalogSessionResume>> {
    validate_project_bucket(project_bucket)?;
    let scan = SessionManager::scan_catalog(sessions_root);
    report_catalog_diagnostics(&scan.diagnostics);
    let Some(entry) = scan
        .entries
        .iter()
        .find(|entry| entry.project_bucket == project_bucket && entry.id == id)
    else {
        let project_diagnostics: Vec<_> = scan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.project_bucket.as_deref() == Some(project_bucket))
            .cloned()
            .collect();
        reject_matching_catalog_diagnostic(&project_diagnostics, id)?;
        return Ok(None);
    };
    let manager = SessionManager::with_root(sessions_root.join(project_bucket));
    let lease = manager.acquire_lease(id)?;
    let outcome = converge_session(&manager, &lease)?;
    Ok(Some(PreparedCatalogSessionResume {
        project_bucket: entry.project_bucket.clone(),
        view: CatalogSessionView {
            snapshot: outcome.snapshot,
            meta: outcome.meta,
            presentation: outcome.presentation,
        },
        lease,
    }))
}

fn load_catalog_session_view_in_project_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<Option<CatalogSessionView>> {
    validate_project_bucket(project_bucket)?;
    let scan = SessionManager::scan_catalog(sessions_root);
    report_catalog_diagnostics(&scan.diagnostics);
    let entry = scan
        .entries
        .iter()
        .find(|entry| entry.project_bucket == project_bucket && entry.id == id);
    if entry.is_none() {
        let project_diagnostics: Vec<_> = scan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.project_bucket.as_deref() == Some(project_bucket))
            .cloned()
            .collect();
        reject_matching_catalog_diagnostic(&project_diagnostics, id)?;
    }
    entry
        .map(|entry| load_catalog_session_view_in_root(sessions_root, entry))
        .transpose()
}

fn load_catalog_session_view_in_root(
    sessions_root: &std::path::Path,
    entry: &atomcode_capabilities::session::CatalogEntry,
) -> anyhow::Result<CatalogSessionView> {
    use atomcode_capabilities::session::CatalogPresence;

    validate_project_bucket(&entry.project_bucket)?;
    let manager = SessionManager::with_root(sessions_root.join(&entry.project_bucket));
    let meta = optional_store(manager.read_meta(&entry.id))?;
    if meta.as_ref().map(|meta| &meta.owner) == Some(&StorageOwner::Native) {
        let mut loaded = manager.load_native_session(&entry.id)?;
        let old_name = loaded.meta.name.clone();
        loaded
            .meta
            .auto_name_from_messages(&loaded.snapshot.messages);
        if loaded.meta.name != old_name {
            manager.update_meta(&entry.id, |meta| {
                meta.auto_name_from_messages(&loaded.snapshot.messages);
            })?;
            loaded.meta = manager.read_meta(&entry.id)?;
        }
        return Ok(loaded.into());
    }

    if entry.presence == CatalogPresence::NativeOnly {
        let meta =
            meta.ok_or_else(|| anyhow::anyhow!("native session {:?} has no metadata", entry.id))?;
        if meta.owner != StorageOwner::Unconfirmed {
            anyhow::bail!(
                "native-only session {:?} has incompatible owner {:?}",
                entry.id,
                meta.owner
            )
        }
        // Pre-owner native sessions have a valid meta + snapshot but no
        // presentation sidecar. Resolve that historical state through the same
        // lease-protected convergence seam used by runtime startup, so readers
        // only ever receive a complete owner=native aggregate.
        let lease = manager.acquire_lease(&entry.id)?;
        let adopted = converge_session(&manager, &lease)?;
        return Ok(CatalogSessionView {
            snapshot: adopted.snapshot,
            presentation: adopted.presentation,
            meta: adopted.meta,
        });
    }

    let bytes = manager.read_legacy_bytes(&entry.id)?;
    let legacy: LegacySession = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid legacy session {:?}: {error}", entry.id))?;
    if legacy.id != entry.id {
        anyhow::bail!(
            "legacy filename id {:?} does not match stored id {:?}",
            entry.id,
            legacy.id
        )
    }
    Ok(convert_legacy_session(&legacy)?.into())
}

pub fn rename_catalog_session_in_project(
    project_bucket: &str,
    id: &str,
    new_name: &str,
) -> anyhow::Result<String> {
    rename_catalog_session_in_project_root(
        &SessionManager::sessions_root(),
        project_bucket,
        id,
        new_name,
        false,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!("session {project_bucket}/{id} rejected a user rename unexpectedly")
    })
}

pub fn apply_ai_catalog_name_in_project(
    project_bucket: &str,
    id: &str,
    new_name: &str,
) -> anyhow::Result<bool> {
    Ok(rename_catalog_session_in_project_root(
        &SessionManager::sessions_root(),
        project_bucket,
        id,
        new_name,
        true,
    )?
    .is_some())
}

fn rename_catalog_session_in_project_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
    new_name: &str,
    ai: bool,
) -> anyhow::Result<Option<String>> {
    validate_project_bucket(project_bucket)?;
    let scan = SessionManager::scan_catalog(sessions_root);
    report_catalog_diagnostics(&scan.diagnostics);
    let entry = scan
        .entries
        .iter()
        .find(|entry| entry.project_bucket == project_bucket && entry.id == id)
        .ok_or_else(|| {
            let project_diagnostics: Vec<_> = scan
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.project_bucket.as_deref() == Some(project_bucket))
                .cloned()
                .collect();
            reject_matching_catalog_diagnostic(&project_diagnostics, id)
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("session {project_bucket}/{id} not found"))
        })?;
    rename_catalog_entry_in_root(sessions_root, entry, new_name, ai)
}

/// Delete every persisted representation of a session under the same active-session
/// lease used by native runtimes. The project bucket is an external API value, so it
/// must be validated before it is joined below the sessions root.
pub fn delete_catalog_session_in_project(project_bucket: &str, id: &str) -> anyhow::Result<()> {
    delete_catalog_session_in_root(&SessionManager::sessions_root(), project_bucket, id)
}

/// Delete a project's catalog directory under sessions root
pub fn delete_catalog_project_in_project(project_bucket: &str) -> anyhow::Result<()> {
    validate_project_bucket(project_bucket)?;
    let bucket_dir = SessionManager::sessions_root().join(project_bucket);
    if bucket_dir.exists() {
        std::fs::remove_dir_all(bucket_dir)?;
    }
    Ok(())
}

/// Append UI-only text without ever inserting it into the runtime snapshot. Native
/// sessions use the stable turn-anchored presentation sidecar; legacy sessions keep
/// their historical JSON representation until S4b performs cutover.
pub fn append_catalog_presentation_in_project(
    project_bucket: &str,
    id: &str,
    messages: &[(PresentationRole, String)],
) -> anyhow::Result<usize> {
    append_catalog_presentation_in_root(
        &SessionManager::sessions_root(),
        project_bucket,
        id,
        messages,
    )
}

/// Persist a terminal snapshot for the narrow case where a `/chat` request is
/// cancelled before its runtime starts. Normal turns are persisted by native hooks.
pub fn persist_pre_runtime_terminal(
    working_dir: &std::path::Path,
    id: &str,
    snapshot: &atomcode_kernel::message::SessionSnapshot,
) -> anyhow::Result<()> {
    let manager = SessionManager::for_project(working_dir);
    let lease = manager.acquire_lease(id)?;
    let has_existing = [
        manager.meta_path(id)?,
        manager.snapshot_path(id)?,
        manager.legacy_path(id)?,
    ]
    .iter()
    .any(|path| path.exists());
    let mut native_snapshot = snapshot.clone();
    if has_existing {
        let outcome = converge_session(&manager, &lease)?;
        native_snapshot.turn_counter = native_snapshot
            .turn_counter
            .max(outcome.snapshot.turn_counter);
        native_snapshot.request_counter = native_snapshot
            .request_counter
            .max(outcome.snapshot.request_counter);
        let message_count = u32::try_from(native_snapshot.messages.len())?;
        let updated_at = atomcode_capabilities::session::now_ms();
        manager.commit_native_runtime_mutation(&lease, &native_snapshot, move |_, meta, _| {
            meta.message_count = message_count;
            meta.updated_at = updated_at;
            Ok(())
        })?;
    } else {
        let now = atomcode_capabilities::session::now_ms();
        let mut meta = SessionMeta::new(id, working_dir.to_string_lossy(), now);
        meta.owner = StorageOwner::Native;
        meta.message_count = u32::try_from(native_snapshot.messages.len())?;
        manager.commit_native_import(
            &lease,
            Some(&native_snapshot),
            Some(&PresentationFile::default()),
            &meta,
        )?;
    }
    Ok(())
}

fn append_catalog_presentation_in_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
    messages: &[(PresentationRole, String)],
) -> anyhow::Result<usize> {
    validate_project_bucket(project_bucket)?;
    let manager = SessionManager::with_root(sessions_root.join(project_bucket));
    let meta = optional_store(manager.read_meta(id))?;
    let legacy = optional_store(manager.read_legacy_bytes(id))?;
    let mut cutover_lease = None;
    match meta {
        Some(meta) if meta.owner == StorageOwner::Native => {}
        Some(_) => {
            let lease = manager.acquire_lease(id)?;
            converge_session(&manager, &lease)?;
            cutover_lease = Some(lease);
        }
        None if legacy.is_some() => {
            let lease = manager.acquire_lease(id)?;
            converge_session(&manager, &lease)?;
            cutover_lease = Some(lease);
        }
        None => anyhow::bail!("session {project_bucket}/{id} not found"),
    }
    let count = manager.append_presentation_at_latest_valid_turn(id, messages)?;
    drop(cutover_lease);
    Ok(count)
}

fn delete_catalog_session_in_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<()> {
    validate_project_bucket(project_bucket)?;
    let manager = SessionManager::with_root(sessions_root.join(project_bucket));
    let lease = manager.acquire_lease(id)?;
    let targets = [
        manager.snapshot_path(id)?,
        manager.meta_path(id)?,
        manager.jsonl_path(id)?,
        manager.presentation_path(id)?,
        manager.legacy_path(id)?,
    ];
    let mut found = false;
    for path in &targets {
        found |= path.try_exists()?;
    }
    if !found {
        return Err(SessionStoreError::NotFound {
            path: manager.meta_path(id)?,
        }
        .into());
    }
    manager.delete(&lease)?;
    Ok(())
}

fn validate_project_bucket(project_bucket: &str) -> anyhow::Result<()> {
    if project_bucket.len() != 16 || !project_bucket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid project bucket {project_bucket:?}")
    }
    Ok(())
}

fn report_catalog_diagnostics(diagnostics: &[atomcode_capabilities::session::CatalogDiagnostic]) {
    for diagnostic in diagnostics {
        tracing::warn!(
            path = %diagnostic.path.display(),
            kind = ?diagnostic.kind,
            message = %diagnostic.message,
            "session catalog entry was skipped"
        );
    }
}

fn reject_matching_catalog_diagnostic(
    diagnostics: &[atomcode_capabilities::session::CatalogDiagnostic],
    query: &str,
) -> anyhow::Result<()> {
    let mut matches = diagnostics.iter().filter(|diagnostic| {
        catalog_diagnostic_session_id(&diagnostic.path)
            .is_some_and(|id| id == query || id.starts_with(query))
    });
    let Some(first) = matches.next() else {
        return Ok(());
    };
    if matches.next().is_some() {
        anyhow::bail!("session query {query:?} matches multiple damaged catalog entries")
    }
    anyhow::bail!("{}: {}", first.path.display(), first.message)
}

fn catalog_diagnostic_session_id(path: &std::path::Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    [".ui.json", ".snapshot", ".meta", ".jsonl", ".json"]
        .into_iter()
        .find_map(|suffix| name.strip_suffix(suffix))
}

fn rename_catalog_entry_in_root(
    sessions_root: &std::path::Path,
    entry: &atomcode_capabilities::session::CatalogEntry,
    new_name: &str,
    ai: bool,
) -> anyhow::Result<Option<String>> {
    use atomcode_capabilities::session::CatalogPresence;

    let manager = SessionManager::with_root(sessions_root.join(&entry.project_bucket));
    let native_meta = optional_store(manager.read_meta(&entry.id))?;
    let use_native = entry.presence == CatalogPresence::NativeOnly
        || native_meta.as_ref().map(|meta| &meta.owner) == Some(&StorageOwner::Native);
    let mut cutover_lease = None;
    let meta = if use_native {
        native_meta
            .ok_or_else(|| anyhow::anyhow!("native session {:?} has no metadata", entry.id))?
    } else {
        let lease = manager.acquire_lease(&entry.id)?;
        let outcome = converge_session(&manager, &lease)?;
        cutover_lease = Some(lease);
        outcome.meta
    };
    if ai {
        let old_name = manager.update_meta(&entry.id, |meta| {
            if !atomcode_coding::session_title::should_accept_ai_name(
                meta.user_renamed,
                meta.ai_named,
            ) {
                return None;
            }
            let old_name = std::mem::replace(&mut meta.name, new_name.to_string());
            meta.ai_named = true;
            meta.updated_at = atomcode_capabilities::session::now_ms();
            Some(old_name)
        })?;
        drop(cutover_lease);
        return Ok(old_name);
    } else {
        manager.rename(&entry.id, new_name)?;
    }
    let old_name = meta.name;
    drop(cutover_lease);
    Ok(Some(old_name))
}

fn optional_store<T>(result: SessionResult<T>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn optional_replaceable_legacy_sidecar<T>(result: SessionResult<T>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(SessionStoreError::Corrupt { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn rebase_converted_turn_ids(
    converted: &mut ConvertedLegacySession,
    base: u64,
) -> anyhow::Result<()> {
    if base == 0 {
        return Ok(());
    }
    for stat in &mut converted.meta.turn_stats {
        stat.turn_id = stat
            .turn_id
            .checked_add(base)
            .ok_or_else(|| anyhow::anyhow!("imported turn id overflow"))?;
    }
    for entry in &mut converted.presentation.entries {
        if let atomcode_capabilities::session::DisplayAnchor::AfterTurn { turn_id } =
            &mut entry.anchor
        {
            *turn_id = turn_id
                .checked_add(base)
                .ok_or_else(|| anyhow::anyhow!("presentation turn id overflow"))?;
        }
    }
    converted.snapshot.turn_counter = converted
        .snapshot
        .turn_counter
        .checked_add(base)
        .ok_or_else(|| anyhow::anyhow!("snapshot turn counter overflow"))?;
    Ok(())
}

fn repair_metadata_only_sidecars(
    legacy_bytes: &[u8],
    native_message_count: usize,
    meta: &mut SessionMeta,
    presentation: &PresentationFile,
) -> anyhow::Result<Option<ImportDiagnostic>> {
    let Some(info) = meta.import_info.as_ref() else {
        return Ok(None);
    };
    if info.kind != ImportKind::MetadataOnly || info.importer_version >= IMPORTER_VERSION {
        return Ok(None);
    }
    if info.legacy_schema != LEGACY_SCHEMA || info.source_sha256 != sha256_hex(legacy_bytes) {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    }
    let importer_version = info.importer_version;
    if importer_version == 3 {
        tracing::warn!(
            session_id = %meta.id,
            "re-auditing metadata-only importer v3 output; presentation removed by the old v3 importer cannot be reconstructed automatically"
        );
    }
    let legacy: LegacySession = serde_json::from_slice(legacy_bytes)
        .map_err(|error| anyhow::anyhow!("invalid legacy session {:?}: {error}", meta.id))?;
    if legacy.id != meta.id {
        anyhow::bail!(
            "legacy filename id {:?} does not match stored id {:?}",
            meta.id,
            legacy.id
        )
    }
    let (converted, _) = convert_legacy_session_with_diagnostic(&legacy)?;
    let expected = &converted.meta.turn_stats;
    if expected.is_empty() {
        if let Some(info) = &mut meta.import_info {
            info.importer_version = IMPORTER_VERSION;
        }
        return Ok(Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
            repaired_turn_stats: 0,
            removed_presentation_entries: 0,
        }));
    }

    if meta.turn_stats.len() < expected.len() {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    }
    let (candidate, native_suffix) = meta.turn_stats.split_at(expected.len());
    let Some(turn_id_offset) = candidate[0].turn_id.checked_sub(expected[0].turn_id) else {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    };
    let prefix_matches = candidate.iter().zip(expected).all(|(stored, legacy)| {
        legacy.turn_id.checked_add(turn_id_offset) == Some(stored.turn_id)
            && stored.after_message == legacy.after_message
            && stored.round_count == legacy.round_count
            && stored.tool_call_count == legacy.tool_call_count
            && stored.duration_ms == legacy.duration_ms
            && stored.total_tokens == legacy.total_tokens
            && stored.errored == legacy.errored
            && stored.used_tokens == legacy.used_tokens
            && stored.ctx_window == legacy.ctx_window
    });
    if !prefix_matches {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    }

    let origin_is_proven = match importer_version {
        1 => {
            candidate.iter().all(|stat| stat.position_valid)
                && candidate
                    .iter()
                    .any(|stat| stat.after_message > native_message_count)
                && native_suffix.iter().any(|stat| {
                    stat.position_valid
                        && stat.turn_id != 0
                        && stat.after_message <= native_message_count
                })
        }
        2 | 3 => candidate.iter().all(|stat| !stat.position_valid),
        _ => false,
    };
    if !origin_is_proven {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    }

    let imported_turn_ids: std::collections::BTreeSet<_> =
        candidate.iter().map(|stat| stat.turn_id).collect();
    let presentation_has_imported_anchor = presentation.entries.iter().any(|entry| {
        matches!(
            entry.anchor,
            atomcode_capabilities::session::DisplayAnchor::AfterTurn { turn_id }
                if imported_turn_ids.contains(&turn_id)
        )
    });
    if presentation_has_imported_anchor {
        return Ok(Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved));
    }
    for stat in &mut meta.turn_stats[..expected.len()] {
        stat.position_valid = false;
    }
    if let Some(info) = &mut meta.import_info {
        info.importer_version = IMPORTER_VERSION;
    }
    Ok(Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
        repaired_turn_stats: expected.len(),
        removed_presentation_entries: 0,
    }))
}

fn seconds_to_millis(seconds: u64, field: &str) -> anyhow::Result<i64> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or_else(|| anyhow::anyhow!("legacy {field} overflows epoch milliseconds"))
}

fn checked_u32(value: usize, field: &str) -> anyhow::Result<u32> {
    u32::try_from(value).map_err(|_| anyhow::anyhow!("legacy {field} exceeds u32"))
}

fn validate_tool_pairing(messages: &[KernelMessage]) -> anyhow::Result<()> {
    use std::collections::HashSet;

    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    for message in messages {
        if message.role == KernelRole::Tool {
            let call_id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("legacy tool pairing has a result without id"))?;
            if !pending.remove(call_id) {
                anyhow::bail!("legacy tool pairing has an orphan or duplicate result: {call_id}")
            }
            continue;
        }

        if !pending.is_empty() {
            anyhow::bail!("legacy tool pairing has dangling calls before the next message")
        }
        for call in &message.tool_calls {
            if !seen.insert(call.id.as_str()) || !pending.insert(call.id.as_str()) {
                anyhow::bail!("legacy tool pairing has a duplicate call id: {}", call.id)
            }
        }
    }
    if !pending.is_empty() {
        anyhow::bail!("legacy tool pairing has dangling calls at end of history")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inline fixture used by the DTO-decoupling characterization test.
    /// Covers: AssistantWithToolCalls (tool_calls + reasoning_content + thinking_blocks),
    /// ToolResult, ToolResultRef, MultiPart (with image), cold_summaries, display_messages,
    /// turn_stats, user_renamed:true, seconds-level created_at/updated_at.
    const LEGACY_JSON: &str = include_str!("../tests/fixtures/session/legacy_full.json");

    fn full_legacy_session() -> LegacySession {
        serde_json::from_str(LEGACY_JSON).expect("full legacy session fixture must parse")
    }

    /// Characterization (baseline) test: locks the current importer output so that
    /// the DTO decoupling in Steps 4-5 is proven to be a pure type substitution.
    #[test]
    fn legacy_import_is_stable_across_dto_decoupling() {
        let session: LegacySession = serde_json::from_str(LEGACY_JSON).unwrap();
        let out = convert_legacy_session(&session).expect("fixture must convert");

        // kernel snapshot: 2 cold-summary synthetics + 7 real messages = 9
        assert_eq!(out.snapshot.messages.len(), 9);
        // cold-summary synthetic messages carry the legacy origin marker
        assert!(out
            .snapshot
            .messages
            .iter()
            .any(|m| { m.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN) }));
        // meta: naming flags and seconds → milliseconds timestamp conversion
        assert_eq!(out.meta.user_renamed, true);
        assert_eq!(out.meta.created_at, session.created_at as i64 * 1000);
        // presentation: the fixture has 2 display_messages
        assert_eq!(out.presentation.entries.len(), 2);
    }

    #[test]
    fn catalog_diagnostics_only_block_the_damaged_session() {
        use atomcode_capabilities::session::{CatalogDiagnostic, CatalogDiagnosticKind};

        let diagnostic = CatalogDiagnostic {
            project_bucket: Some("0123456789abcdef".into()),
            path: std::path::PathBuf::from("/sessions/0123456789abcdef/damaged.snapshot"),
            kind: CatalogDiagnosticKind::Corrupt,
            message: "sidecars but no metadata".into(),
        };

        reject_matching_catalog_diagnostic(std::slice::from_ref(&diagnostic), "healthy")
            .expect("unrelated valid sessions must remain usable");
        let error = reject_matching_catalog_diagnostic(&[diagnostic], "damaged").unwrap_err();
        assert!(error.to_string().contains("sidecars but no metadata"));
    }

    #[test]
    fn prepared_resume_keeps_exact_bucket_and_holds_cutover_lease() {
        let root = tempfile::tempdir().unwrap();
        let id = "same-id";
        let first_bucket = "1111111111111111";
        let second_bucket = "2222222222222222";
        for (bucket, text) in [(first_bucket, "first"), (second_bucket, "second")] {
            let manager = SessionManager::with_root(root.path().join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot =
                atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(text)]);
            let mut meta = SessionMeta::new(id, format!("/{text}"), 1);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        }

        let prepared =
            prepare_catalog_session_resume_in_project_root(root.path(), first_bucket, id)
                .unwrap()
                .unwrap();

        assert_eq!(prepared.project_bucket, first_bucket);
        assert_eq!(prepared.view.snapshot.messages[0].text, "first");
        let first = SessionManager::with_root(root.path().join(first_bucket));
        let second = SessionManager::with_root(root.path().join(second_bucket));
        assert!(matches!(
            first.acquire_lease(id),
            Err(atomcode_capabilities::session::SessionStoreError::SessionInUse { .. })
        ));
        assert!(second.acquire_lease(id).is_ok());

        drop(prepared);
        assert!(first.acquire_lease(id).is_ok());
    }

    #[test]
    fn prepared_resume_repairs_out_of_range_legacy_boundary_before_lease_transfer() {
        let root = tempfile::tempdir().unwrap();
        let bucket = "3333333333333333";
        let mut legacy = full_legacy_session();
        legacy.turn_stats[0].after_message = legacy.messages.len() + 1;
        let manager = SessionManager::with_root(root.path().join(bucket));
        std::fs::create_dir_all(manager.root()).unwrap();
        std::fs::write(
            manager.legacy_path(&legacy.id).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let prepared =
            prepare_catalog_session_resume_in_project_root(root.path(), bucket, &legacy.id)
                .unwrap()
                .unwrap();

        assert_eq!(prepared.view.meta.owner, StorageOwner::Native);
        assert!(prepared
            .view
            .meta
            .turn_stats
            .iter()
            .all(|stat| stat.after_message <= prepared.view.snapshot.messages.len()));
        assert!(matches!(
            manager.acquire_lease(&legacy.id),
            Err(atomcode_capabilities::session::SessionStoreError::SessionInUse { .. })
        ));
    }

    #[test]
    fn prepared_resume_accepts_missing_legacy_turn_count() {
        let root = tempfile::tempdir().unwrap();
        let bucket = "4444444444444444";
        let mut legacy = full_legacy_session();
        legacy.turn_stats[1].turn_count = None;
        let manager = SessionManager::with_root(root.path().join(bucket));
        std::fs::create_dir_all(manager.root()).unwrap();
        std::fs::write(
            manager.legacy_path(&legacy.id).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let prepared =
            prepare_catalog_session_resume_in_project_root(root.path(), bucket, &legacy.id)
                .unwrap()
                .unwrap();

        assert_eq!(prepared.view.meta.owner, StorageOwner::Native);
        assert!(!prepared.view.snapshot.messages.is_empty());
        assert_eq!(prepared.view.meta.turn_stats.len(), 2);
        assert_eq!(prepared.view.meta.turn_stats[1].round_count, 0);
    }

    #[test]
    fn full_legacy_fixture_converts_to_expected_kernel_snapshot() {
        let session = full_legacy_session();
        // Build the kernel snapshot via the frozen-DTO path (mirrors what
        // convert_legacy_session does internally after the DTO decoupling).
        let mut msgs = Vec::new();
        for s in &session.cold_summaries {
            let mut m = KernelMessage::user(format!("{LEGACY_COLD_SUMMARY_PREFIX}{s}"));
            m.synthetic = true;
            m.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
            msgs.push(m);
        }
        msgs.extend(session.messages.iter().map(legacy_message_to_kernel));
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(msgs);

        assert_eq!(snapshot.version, atomcode_kernel::message::SNAPSHOT_VERSION);
        assert_eq!(snapshot.messages.len(), 9);
        assert_eq!(snapshot.cache_epoch, 0);
        assert_eq!((snapshot.turn_counter, snapshot.request_counter), (0, 0));

        for (message, summary) in snapshot.messages[..2]
            .iter()
            .zip(["older summary one", "older summary two"])
        {
            assert!(message.synthetic);
            assert_eq!(
                message.internal_origin.as_deref(),
                Some(LEGACY_COLD_SUMMARY_ORIGIN)
            );
            assert_eq!(
                message.text,
                format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}")
            );
        }

        let image_message = &snapshot.messages[3];
        assert_eq!(image_message.role, KernelRole::User);
        assert_eq!(image_message.text, "inspect this image");
        assert_eq!(image_message.images.len(), 1);
        assert_eq!(image_message.images[0].media_type, "image/png");
        assert_eq!(image_message.images[0].data, "aW1hZ2UtZml4dHVyZQ==");

        let reasoning_message = &snapshot.messages[4];
        assert_eq!(
            reasoning_message.reasoning.as_deref(),
            Some("plain reasoning")
        );
        assert_eq!(reasoning_message.reasoning_blocks.len(), 1);
        assert_eq!(
            reasoning_message.reasoning_blocks[0].text,
            "signed reasoning"
        );
        assert_eq!(
            reasoning_message.reasoning_blocks[0].opaque.as_deref(),
            Some("anthropic-signature")
        );
        assert_eq!(
            reasoning_message.reasoning_blocks[0].provider.as_deref(),
            None
        );

        let referenced_result = &snapshot.messages[7];
        assert_eq!(referenced_result.role, KernelRole::Tool);
        assert_eq!(referenced_result.tool_call_id.as_deref(), Some("call-ref"));
        assert_eq!(referenced_result.text, "cached failure summary");
        assert!(referenced_result.is_error);

        let synthetic = &snapshot.messages[8];
        assert!(synthetic.synthetic);
        assert_eq!(synthetic.internal_origin.as_deref(), Some("verify_cadence"));
    }

    #[test]
    fn full_legacy_fixture_converts_runtime_catalog_and_presentation_together() {
        use atomcode_capabilities::session::{DisplayAnchor, PresentationRole};

        let session = full_legacy_session();
        let converted = convert_legacy_session(&session).expect("fixture must convert");

        assert_eq!(converted.snapshot.messages.len(), 9);
        assert_eq!(converted.snapshot.turn_counter, 2);
        assert_eq!(converted.snapshot.request_counter, 0);

        assert_eq!(converted.meta.id, session.id.as_str());
        assert_eq!(converted.meta.name, "legacy-full");
        assert!(converted.meta.user_renamed);
        assert!(converted.meta.ai_named);
        assert_eq!(converted.meta.created_at, 1_700_000_000_000);
        assert_eq!(converted.meta.updated_at, 1_700_000_123_000);
        assert_eq!(converted.meta.turn_count, 2);
        assert_eq!(converted.meta.message_count, 7);
        assert_eq!(converted.meta.turn_stats[0].turn_id, 1);
        assert_eq!(converted.meta.turn_stats[0].round_count, 2);
        assert_eq!(converted.meta.turn_stats[1].turn_id, 2);

        assert_eq!(converted.presentation.entries.len(), 2);
        assert_eq!(
            converted.presentation.entries[0].anchor,
            DisplayAnchor::AtStart
        );
        assert_eq!(
            converted.presentation.entries[0].role,
            PresentationRole::Assistant
        );
        assert_eq!(converted.presentation.entries[0].text, "local preamble");
        assert_eq!(
            converted.presentation.entries[1].anchor,
            DisplayAnchor::AfterTurn { turn_id: 2 }
        );
        assert_eq!(
            converted.presentation.entries[1].role,
            PresentationRole::User
        );
        assert_eq!(converted.presentation.entries[1].text, "local note");
    }

    #[test]
    fn minimal_legacy_fixture_uses_additive_defaults() {
        let session: LegacySession = serde_json::from_str(include_str!(
            "../tests/fixtures/session/legacy_minimal.json"
        ))
        .expect("minimal legacy session fixture must parse");
        let converted = convert_legacy_session(&session).expect("fixture must convert");

        assert!(!converted.meta.user_renamed);
        assert!(!converted.meta.ai_named);
        assert!(converted.meta.turn_stats.is_empty());
        assert_eq!(converted.meta.turn_count, 0);
        assert_eq!(converted.meta.message_count, 2);
        assert!(converted.presentation.entries.is_empty());
        assert_eq!(converted.snapshot.turn_counter, 0);
        assert_eq!(converted.snapshot.request_counter, 0);
    }

    #[test]
    fn importer_rejects_dangling_legacy_tool_call() {
        let mut session = full_legacy_session();
        session.messages[3] = LegacyMessage {
            role: "User".to_string(),
            content: LegacyContent::Text("interrupt".to_string()),
            synthetic: false,
            internal_origin: None,
        };

        let error = convert_legacy_session(&session).unwrap_err();
        assert!(error.to_string().contains("tool pairing"), "{error:#}");
    }

    #[test]
    fn legacy_only_cutover_commits_once_and_is_idempotent() {
        use atomcode_capabilities::session::{SessionManager, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let session = full_legacy_session();
        let id = session.id.as_str();
        std::fs::write(
            manager.legacy_path(id).unwrap(),
            include_bytes!("../tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let first = converge_session(&manager, &lease).unwrap();
        assert_eq!(first.status, ImportStatus::ImportedFull);
        assert_eq!(first.meta.owner, StorageOwner::Native);
        assert!(first.meta.import_info.is_some());
        assert!(first.diagnostic.is_none());

        let second = converge_session(&manager, &lease).unwrap();
        assert_eq!(second.status, ImportStatus::AlreadyNative);
        assert_eq!(second.snapshot, first.snapshot);
        assert_eq!(second.meta, first.meta);
    }

    #[test]
    fn legacy_cutover_repairs_placeholder_name_before_native_commit() {
        use atomcode_capabilities::session::{SessionManager, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let mut session = full_legacy_session();
        session.name = format!("session-{}", session.id);
        session.user_renamed = false;
        session.ai_named = false;
        let id = session.id.clone();
        std::fs::write(
            manager.legacy_path(&id).unwrap(),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        let lease = manager.acquire_lease(&id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();

        assert_eq!(imported.meta.owner, StorageOwner::Native);
        assert_eq!(imported.meta.name, "inspect this image");
        assert_eq!(manager.read_meta(&id).unwrap().name, "inspect this image");
    }

    #[test]
    fn valid_native_snapshot_is_never_overwritten_by_legacy() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let session = full_legacy_session();
        let id = session.id.as_str();
        let native = atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
            "native wins",
        )]);
        manager.save_snapshot(id, &native).unwrap();
        manager
            .write_meta(&SessionMeta::new(id, "/native", 7))
            .unwrap();
        std::fs::write(
            manager.legacy_path(id).unwrap(),
            include_bytes!("../tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();
        assert_eq!(imported.status, ImportStatus::ImportedMetadataOnly);
        assert_eq!(imported.snapshot.messages, native.messages);
        assert_eq!(imported.snapshot.cache_epoch, native.cache_epoch);
        assert_eq!(imported.snapshot.request_counter, native.request_counter);
        assert_eq!(imported.snapshot.turn_counter, 2);
        assert!(!imported.meta.turn_stats.is_empty());
        assert!(imported
            .meta
            .turn_stats
            .iter()
            .all(|stat| !stat.position_valid));
        assert!(
            imported.presentation.entries.is_empty(),
            "metadata-only import must not mix legacy presentation into the native snapshot"
        );
        assert!(manager.read_presentation(id).unwrap().entries.is_empty());
        assert_eq!(manager.load_snapshot(id).unwrap(), imported.snapshot);
    }

    #[test]
    fn empty_unconfirmed_native_stub_is_replaced_by_populated_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let empty_snapshot = atomcode_kernel::message::SessionSnapshot::new(Vec::new());
        let empty_presentation = PresentationFile::default();
        manager.save_snapshot(&legacy.id, &empty_snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &empty_presentation)
            .unwrap();
        manager
            .write_meta(&SessionMeta::new(&legacy.id, "/native", 7))
            .unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();

        assert_eq!(imported.status, ImportStatus::ImportedFull);
        assert!(!imported.snapshot.messages.is_empty());
        assert_eq!(
            manager.load_native_session(&legacy.id).unwrap().snapshot,
            imported.snapshot
        );
    }

    #[test]
    fn empty_metadata_only_native_stub_recovers_from_matching_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let empty_snapshot = atomcode_kernel::message::SessionSnapshot::new(Vec::new());
        let empty_presentation = PresentationFile::default();
        let mut poisoned_meta = SessionMeta::new(&legacy.id, "/native", 7);
        poisoned_meta.owner = StorageOwner::Native;
        poisoned_meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: IMPORTER_VERSION,
            kind: ImportKind::MetadataOnly,
        });
        manager.save_snapshot(&legacy.id, &empty_snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &empty_presentation)
            .unwrap();
        manager.write_meta(&poisoned_meta).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let recovered = converge_session(&manager, &lease).unwrap();

        assert_eq!(recovered.status, ImportStatus::ImportedFull);
        assert!(!recovered.snapshot.messages.is_empty());
        assert_eq!(
            recovered.meta.import_info.as_ref().map(|info| &info.kind),
            Some(&ImportKind::Full)
        );
        assert_eq!(
            manager.load_native_session(&legacy.id).unwrap().snapshot,
            recovered.snapshot
        );
    }

    #[test]
    fn empty_native_session_without_matching_import_provenance_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let empty_snapshot = atomcode_kernel::message::SessionSnapshot::new(Vec::new());
        let empty_presentation = PresentationFile::default();
        let mut native_meta = SessionMeta::new(&legacy.id, "/native", 7);
        native_meta.owner = StorageOwner::Native;
        manager.save_snapshot(&legacy.id, &empty_snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &empty_presentation)
            .unwrap();
        manager.write_meta(&native_meta).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let loaded = converge_session(&manager, &lease).unwrap();

        assert_eq!(loaded.status, ImportStatus::AlreadyNative);
        assert!(loaded.snapshot.messages.is_empty());
        assert_eq!(manager.read_meta(&legacy.id).unwrap(), native_meta);
    }

    #[test]
    fn full_legacy_import_replaces_an_orphan_presentation_from_an_incomplete_native_state() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let expected = convert_legacy_session(&legacy).unwrap().presentation;
        let orphan = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "orphan native sidecar".into(),
            }],
        };
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        std::fs::write(
            manager.presentation_path(&legacy.id).unwrap(),
            serde_json::to_vec(&orphan).unwrap(),
        )
        .unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();

        assert_eq!(imported.status, ImportStatus::ImportedFull);
        assert_eq!(imported.presentation, expected);
        assert_eq!(manager.read_presentation(&legacy.id).unwrap(), expected);
    }

    #[test]
    fn metadata_only_import_preserves_an_existing_native_presentation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let native_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
                "native wins",
            )]);
        let native_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "native preamble".into(),
            }],
        };
        manager.save_snapshot(&legacy.id, &native_snapshot).unwrap();
        manager
            .write_meta(&SessionMeta::new(&legacy.id, "/native", 7))
            .unwrap();
        std::fs::write(
            manager.presentation_path(&legacy.id).unwrap(),
            serde_json::to_vec(&native_presentation).unwrap(),
        )
        .unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();

        assert_eq!(imported.status, ImportStatus::ImportedMetadataOnly);
        assert_eq!(imported.snapshot.messages, native_snapshot.messages);
        assert_eq!(imported.snapshot.cache_epoch, native_snapshot.cache_epoch);
        assert_eq!(
            imported.snapshot.request_counter,
            native_snapshot.request_counter
        );
        assert_eq!(imported.presentation, native_presentation);
        assert_eq!(
            manager.read_presentation(&legacy.id).unwrap(),
            native_presentation
        );
    }

    #[test]
    fn metadata_only_import_recomputes_after_full_state_cas_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let (converted, diagnostic) = convert_legacy_session_with_diagnostic(&legacy).unwrap();
        let mut stale_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("stale")]);
        stale_snapshot.turn_counter = 1;
        let mut fresh_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("fresh")]);
        fresh_snapshot.turn_counter = 10;
        let stale_presentation = PresentationFile::default();
        let fresh_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "fresh presentation".into(),
            }],
        };
        let stale_meta = SessionMeta::new(&legacy.id, "/native", 1);
        manager.save_snapshot(&legacy.id, &stale_snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &stale_presentation)
            .unwrap();
        manager.write_meta(&stale_meta).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        manager
            .rename(&legacy.id, "concurrent metadata-only title")
            .unwrap();
        manager.save_snapshot(&legacy.id, &fresh_snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &fresh_presentation)
            .unwrap();

        let imported = import_metadata_only_with_cas(
            &manager,
            &lease,
            legacy_bytes,
            &converted,
            diagnostic,
            stale_meta,
            Some(stale_snapshot),
            Some(stale_presentation),
        )
        .unwrap();

        assert_eq!(imported.status, ImportStatus::ImportedMetadataOnly);
        assert_eq!(imported.meta.name, "concurrent metadata-only title");
        assert!(imported.meta.user_renamed);
        assert_eq!(imported.snapshot.messages, fresh_snapshot.messages);
        assert!(imported.snapshot.turn_counter > fresh_snapshot.turn_counter);
        assert_eq!(imported.presentation, fresh_presentation);
        assert_eq!(
            manager.load_native_session(&legacy.id).unwrap().meta,
            imported.meta
        );
    }

    #[test]
    fn metadata_only_import_preserves_native_stats_after_legacy_zero_id_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let (converted, diagnostic) = convert_legacy_session_with_diagnostic(&legacy).unwrap();
        let mut snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        snapshot.turn_counter = 10;
        let presentation = PresentationFile::default();
        let mut meta = SessionMeta::new(&legacy.id, "/native", 1);
        let mut legacy_prefix = converted.meta.turn_stats[0].clone();
        legacy_prefix.turn_id = 0;
        legacy_prefix.position_valid = false;
        let mut native_suffix = converted.meta.turn_stats[0].clone();
        native_suffix.turn_id = 10;
        native_suffix.after_message = 1;
        native_suffix.position_valid = true;
        meta.turn_stats = vec![legacy_prefix, native_suffix.clone()];
        meta.turn_count = 2;
        manager.save_snapshot(&legacy.id, &snapshot).unwrap();
        manager
            .write_presentation(&legacy.id, &presentation)
            .unwrap();
        manager.write_meta(&meta).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let imported = import_metadata_only_with_cas(
            &manager,
            &lease,
            legacy_bytes,
            &converted,
            diagnostic,
            meta,
            Some(snapshot),
            Some(presentation),
        )
        .unwrap();

        assert!(imported.meta.turn_stats.contains(&native_suffix));
        assert_eq!(
            imported.meta.turn_count as usize,
            imported.meta.turn_stats.len()
        );
    }

    #[test]
    fn snapshot_and_legacy_without_meta_is_ambiguous_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let native_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
                "native wins",
            )]);
        manager.save_snapshot(&legacy.id, &native_snapshot).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let error = converge_session(&manager, &lease).unwrap_err();

        assert!(error.to_string().contains("ambiguous"), "{error:#}");
        assert!(!manager.meta_path(&legacy.id).unwrap().exists());
        assert!(!manager.presentation_path(&legacy.id).unwrap().exists());
        assert_eq!(manager.load_snapshot(&legacy.id).unwrap(), native_snapshot);
    }

    #[test]
    fn full_legacy_import_preserves_unconfirmed_user_rename() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let unconfirmed = SessionMeta::new(&legacy.id, "/native", 1);
        manager.write_meta(&unconfirmed).unwrap();
        manager.rename(&legacy.id, "concurrent user title").unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();

        assert_eq!(imported.status, ImportStatus::ImportedFull);
        assert_eq!(imported.meta.name, "concurrent user title");
        assert!(imported.meta.user_renamed);
        assert_eq!(
            manager.load_native_session(&legacy.id).unwrap().meta.name,
            "concurrent user title"
        );
    }

    #[test]
    fn pre_intent_full_import_residue_without_meta_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        manager
            .save_snapshot(&legacy.id, &converted.snapshot)
            .unwrap();
        manager
            .write_presentation(&legacy.id, &converted.presentation)
            .unwrap();
        assert!(!manager.meta_path(&legacy.id).unwrap().exists());
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let error = converge_session(&manager, &lease).unwrap_err();

        assert!(error.to_string().contains("ambiguous"), "{error:#}");
        assert!(!manager.meta_path(&legacy.id).unwrap().exists());
        assert_eq!(
            manager.load_snapshot(&legacy.id).unwrap(),
            converted.snapshot
        );
        assert_eq!(
            manager.read_presentation(&legacy.id).unwrap(),
            converted.presentation
        );
    }

    #[test]
    fn legacy_import_intent_recovers_interrupted_full_import() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        manager
            .save_snapshot(&legacy.id, &converted.snapshot)
            .unwrap();
        manager
            .write_presentation(&legacy.id, &converted.presentation)
            .unwrap();
        let mut intent = converted.meta.clone();
        intent.owner = StorageOwner::Legacy;
        manager.write_meta(&intent).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let recovered = converge_session(&manager, &lease).unwrap();

        assert_eq!(recovered.status, ImportStatus::ImportedFull);
        assert_eq!(recovered.snapshot, converted.snapshot);
        assert_eq!(recovered.presentation, converted.presentation);
        assert_eq!(recovered.meta.owner, StorageOwner::Native);
        assert!(recovered
            .meta
            .turn_stats
            .iter()
            .all(|stat| stat.position_valid));
    }

    #[test]
    fn legacy_import_intent_replaces_corrupt_interrupted_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        std::fs::write(manager.legacy_path(&legacy.id).unwrap(), legacy_bytes).unwrap();
        std::fs::write(
            manager.snapshot_path(&legacy.id).unwrap(),
            b"corrupt snapshot",
        )
        .unwrap();
        std::fs::write(
            manager.presentation_path(&legacy.id).unwrap(),
            b"corrupt presentation",
        )
        .unwrap();
        let mut intent = converted.meta.clone();
        intent.owner = StorageOwner::Legacy;
        manager.write_meta(&intent).unwrap();
        let lease = manager.acquire_lease(&legacy.id).unwrap();

        let recovered = converge_session(&manager, &lease).unwrap();

        assert_eq!(recovered.status, ImportStatus::ImportedFull);
        assert_eq!(recovered.snapshot, converted.snapshot);
        assert_eq!(recovered.presentation, converted.presentation);
        assert_eq!(
            manager.load_native_session(&legacy.id).unwrap().snapshot,
            converted.snapshot
        );
    }

    #[test]
    fn importer_v1_metadata_only_sidecars_are_repaired_without_losing_stats() {
        use atomcode_capabilities::session::{
            ImportInfo, ImportKind, SessionManager, StorageOwner,
        };

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let mut converted = convert_legacy_session(&legacy).unwrap();
        rebase_converted_turn_ids(&mut converted, 5).unwrap();
        let id = legacy.id.as_str();
        let mut snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
                "native wins",
            )]);
        snapshot.turn_counter = converted.snapshot.turn_counter;
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let imported_stat_count = meta.turn_stats.len();
        meta.turn_stats.push(TurnStat {
            after_message: 1,
            position_valid: true,
            turn_id: snapshot.turn_counter + 1,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 17,
            errored: false,
            used_tokens: 1,
            ctx_window: 128,
            model_usage: Vec::new(),
        });
        let native_presentation = PresentationEntry {
            anchor: DisplayAnchor::AfterTurn {
                turn_id: snapshot.turn_counter + 1,
            },
            role: PresentationRole::Assistant,
            text: "native tail".into(),
        };
        let native_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![native_presentation.clone()],
        };
        assert!(meta.turn_stats.iter().all(|stat| stat.position_valid));
        let expected_tokens: u32 = meta.turn_stats.iter().map(|stat| stat.total_tokens).sum();
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&native_presentation), &meta)
            .unwrap();
        std::fs::write(manager.legacy_path(id).unwrap(), legacy_bytes).unwrap();

        let repaired = converge_session(&manager, &lease).unwrap();

        assert_eq!(repaired.status, ImportStatus::AlreadyNative);
        assert_eq!(repaired.snapshot, snapshot);
        assert_eq!(
            repaired
                .meta
                .turn_stats
                .iter()
                .map(|stat| stat.total_tokens)
                .sum::<u32>(),
            expected_tokens
        );
        assert!(repaired.meta.turn_stats[..imported_stat_count]
            .iter()
            .all(|stat| !stat.position_valid));
        assert!(repaired.meta.turn_stats[imported_stat_count].position_valid);
        assert_eq!(repaired.presentation, native_presentation);
        assert_eq!(
            repaired.meta.import_info.as_ref().unwrap().importer_version,
            IMPORTER_VERSION
        );
        assert_eq!(manager.read_meta(id).unwrap(), repaired.meta);
        assert_eq!(
            manager.read_presentation(id).unwrap(),
            repaired.presentation
        );

        let repeated = converge_session(&manager, &lease).unwrap();
        assert_eq!(repeated.diagnostic, None);
        assert_eq!(repeated.meta, repaired.meta);
        assert_eq!(repeated.presentation, repaired.presentation);

        let mut next_snapshot = repeated.snapshot.clone();
        next_snapshot
            .messages
            .push(KernelMessage::user("next turn"));
        let next_message_count = u32::try_from(next_snapshot.messages.len()).unwrap();
        manager
            .commit_native_runtime_mutation(&lease, &next_snapshot, |_, meta, _| {
                meta.message_count = next_message_count;
                Ok(())
            })
            .unwrap();
        let after_turn = manager.read_meta(id).unwrap();
        assert!(after_turn.turn_stats[..imported_stat_count]
            .iter()
            .all(|stat| !stat.position_valid));
        assert_eq!(
            after_turn.import_info.as_ref().unwrap().importer_version,
            IMPORTER_VERSION
        );
        assert_eq!(manager.read_presentation(id).unwrap(), native_presentation);
    }

    #[test]
    fn importer_v2_metadata_only_sidecars_upgrade_without_changing_presentation() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 2,
            kind: ImportKind::MetadataOnly,
        });
        for stat in &mut meta.turn_stats {
            stat.position_valid = false;
        }
        let expected_stat_count = meta.turn_stats.len();
        let mut presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "native preamble".into(),
            }],
        };
        let original_presentation = presentation.clone();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
                repaired_turn_stats: expected_stat_count,
                removed_presentation_entries: 0,
            })
        );
        assert_eq!(presentation, original_presentation);
        assert_eq!(meta.import_info.unwrap().importer_version, IMPORTER_VERSION);
    }

    #[test]
    fn importer_v3_is_reaudited_without_changing_presentation() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 3,
            kind: ImportKind::MetadataOnly,
        });
        for stat in &mut meta.turn_stats {
            stat.position_valid = false;
        }
        let expected_stat_count = meta.turn_stats.len();
        let mut presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "surviving v3 presentation".into(),
            }],
        };
        let original_presentation = presentation.clone();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
                repaired_turn_stats: expected_stat_count,
                removed_presentation_entries: 0,
            })
        );
        assert_eq!(presentation, original_presentation);
        assert!(meta.import_info.unwrap().importer_version > 3);
    }

    #[test]
    fn importer_v3_disk_upgrade_changes_only_metadata_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let id = legacy.id.as_str();
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "surviving v3 presentation".into(),
            }],
        };
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 3,
            kind: ImportKind::MetadataOnly,
        });
        for stat in &mut meta.turn_stats {
            stat.position_valid = false;
        }
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        std::fs::write(manager.legacy_path(id).unwrap(), legacy_bytes).unwrap();
        let snapshot_bytes = std::fs::read(manager.snapshot_path(id).unwrap()).unwrap();
        let presentation_bytes = std::fs::read(manager.presentation_path(id).unwrap()).unwrap();

        let upgraded = converge_session(&manager, &lease).unwrap();

        assert_eq!(
            upgraded.meta.import_info.as_ref().unwrap().importer_version,
            IMPORTER_VERSION
        );
        assert_eq!(
            std::fs::read(manager.snapshot_path(id).unwrap()).unwrap(),
            snapshot_bytes
        );
        assert_eq!(
            std::fs::read(manager.presentation_path(id).unwrap()).unwrap(),
            presentation_bytes
        );
        let upgraded_meta_bytes = std::fs::read(manager.meta_path(id).unwrap()).unwrap();
        let repeated = converge_session(&manager, &lease).unwrap();
        assert_eq!(repeated.diagnostic, None);
        assert_eq!(
            std::fs::read(manager.meta_path(id).unwrap()).unwrap(),
            upgraded_meta_bytes
        );
    }

    #[test]
    fn importer_v2_with_imported_anchor_is_unresolved_and_non_destructive() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 2,
            kind: ImportKind::MetadataOnly,
        });
        for stat in &mut meta.turn_stats {
            stat.position_valid = false;
        }
        let mut presentation = converted.presentation;
        let original_meta = meta.clone();
        let original_presentation = presentation.clone();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
        assert_eq!(presentation, original_presentation);
    }

    #[test]
    fn metadata_only_sidecar_repair_requires_a_strict_stats_prefix() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        meta.turn_stats.insert(
            0,
            TurnStat {
                after_message: 1,
                position_valid: true,
                turn_id: 99,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 128,
                model_usage: Vec::new(),
            },
        );
        let original_meta = meta.clone();
        let mut presentation = PresentationFile::default();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
    }

    #[test]
    fn importer_v1_requires_coordinate_domain_evidence() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let original_meta = meta.clone();
        let mut presentation = PresentationFile::default();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, usize::MAX, &mut meta, &mut presentation)
                .unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
    }

    #[test]
    fn importer_v1_exact_native_prefix_without_native_suffix_is_unresolved() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let original_meta = meta.clone();
        let presentation = PresentationFile::default();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
    }

    #[test]
    fn metadata_only_sidecar_repair_is_non_destructive_when_presentation_origin_is_ambiguous() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let original_meta = meta.clone();
        let mut presentation = converted.presentation;
        presentation.entries.remove(0);
        let original_presentation = presentation.clone();

        let diagnostic = repair_metadata_only_sidecars(
            legacy_bytes,
            converted.snapshot.messages.len(),
            &mut meta,
            &mut presentation,
        )
        .unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
        assert_eq!(presentation, original_presentation);
    }

    #[test]
    fn metadata_only_sidecar_repair_rejects_tail_using_an_imported_turn() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let imported_turn_id = converted.meta.turn_stats.last().unwrap().turn_id;
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let mut presentation = converted.presentation;
        presentation.entries.push(PresentationEntry {
            anchor: DisplayAnchor::AfterTurn {
                turn_id: imported_turn_id,
            },
            role: PresentationRole::Assistant,
            text: "appended after v1 cutover".into(),
        });
        let original_meta = meta.clone();
        let original_presentation = presentation.clone();

        let diagnostic =
            repair_metadata_only_sidecars(legacy_bytes, 1, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
        assert_eq!(presentation, original_presentation);
    }

    #[test]
    fn metadata_only_sidecar_repair_does_not_delete_matching_native_sidecars() {
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        let legacy: LegacySession = serde_json::from_slice(legacy_bytes).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        let native_message_count = converted.snapshot.messages.len();
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let original_meta = meta.clone();
        let mut presentation = converted.presentation;
        let original_presentation = presentation.clone();

        let diagnostic = repair_metadata_only_sidecars(
            legacy_bytes,
            native_message_count,
            &mut meta,
            &mut presentation,
        )
        .unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::MetadataOnlySidecarsUnresolved)
        );
        assert_eq!(meta, original_meta);
        assert_eq!(presentation, original_presentation);
    }

    #[test]
    fn metadata_only_sidecar_repair_does_not_delete_at_start_only_prefix() {
        let mut legacy = full_legacy_session();
        legacy.turn_stats.clear();
        legacy
            .display_messages
            .retain(|display| display.after_message == 0);
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        let converted = convert_legacy_session(&legacy).unwrap();
        assert!(converted.meta.turn_stats.is_empty());
        assert_eq!(converted.presentation.entries.len(), 1);
        assert_eq!(
            converted.presentation.entries[0].anchor,
            DisplayAnchor::AtStart
        );
        let mut meta = converted.meta;
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: LEGACY_SCHEMA.into(),
            source_sha256: sha256_hex(&legacy_bytes),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        let mut presentation = converted.presentation;
        let original_presentation = presentation.clone();

        let diagnostic =
            repair_metadata_only_sidecars(&legacy_bytes, 0, &mut meta, &mut presentation).unwrap();

        assert_eq!(
            diagnostic,
            Some(ImportDiagnostic::RepairedMetadataOnlySidecars {
                repaired_turn_stats: 0,
                removed_presentation_entries: 0,
            })
        );
        assert_eq!(
            meta.import_info.as_ref().unwrap().importer_version,
            IMPORTER_VERSION
        );
        assert_eq!(presentation, original_presentation);
    }

    #[test]
    fn changed_legacy_after_cutover_is_diagnostic_not_overwrite() {
        use atomcode_capabilities::session::SessionManager;

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let session = full_legacy_session();
        let id = session.id.as_str();
        let path = manager.legacy_path(id).unwrap();
        std::fs::write(
            &path,
            include_bytes!("../tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();
        let committed = converge_session(&manager, &lease).unwrap();
        std::fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

        let current = converge_session(&manager, &lease).unwrap();
        assert_eq!(current.status, ImportStatus::AlreadyNative);
        assert_eq!(
            current.diagnostic,
            Some(ImportDiagnostic::LegacyChangedAfterCutover)
        );
        assert_eq!(current.snapshot, committed.snapshot);
    }

    #[test]
    fn owner_native_missing_snapshot_never_falls_back_to_legacy() {
        use atomcode_capabilities::session::{SessionManager, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let session = full_legacy_session();
        let id = session.id.as_str();
        let mut meta = atomcode_capabilities::session::SessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        manager.write_meta(&meta).unwrap();
        std::fs::write(
            manager.legacy_path(id).unwrap(),
            include_bytes!("../tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let error = converge_session(&manager, &lease).unwrap_err();
        let store_error = error
            .downcast_ref::<SessionStoreError>()
            .expect("strict native load must preserve the typed store error");
        assert!(matches!(
            store_error,
            SessionStoreError::NotFound { path }
                if path == &manager.snapshot_path(id).unwrap()
        ));
        assert!(!manager.snapshot_path(id).unwrap().exists());
    }

    #[test]
    fn owner_native_missing_presentation_is_an_explicit_error() {
        use atomcode_capabilities::session::{SessionManager, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let id = "native-missing-presentation";
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        manager.save_snapshot(id, &snapshot).unwrap();
        let mut meta = atomcode_capabilities::session::SessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        manager.write_meta(&meta).unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let error = converge_session(&manager, &lease).unwrap_err();
        let store_error = error
            .downcast_ref::<SessionStoreError>()
            .expect("strict native load must preserve the typed store error");
        assert!(matches!(
            store_error,
            SessionStoreError::NotFound { path }
                if path == &manager.presentation_path(id).unwrap()
        ));
        assert_eq!(manager.load_snapshot(id).unwrap(), snapshot);
    }

    #[test]
    fn complete_unconfirmed_native_without_legacy_is_adopted() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let id = "native-only";
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        manager.save_snapshot(id, &snapshot).unwrap();
        manager.write_meta(&SessionMeta::new(id, "/p", 1)).unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let adopted = converge_session(&manager, &lease).unwrap();
        assert_eq!(adopted.status, ImportStatus::AdoptedNative);
        assert_eq!(adopted.meta.owner, StorageOwner::Native);
        assert!(adopted.meta.import_info.is_none());
        assert_eq!(adopted.snapshot, snapshot);
    }

    #[test]
    fn unconfirmed_adoption_retries_full_state_conflict_without_losing_updates() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta, StorageOwner};

        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let id = "native-adoption-cas";
        let stale_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("stale")]);
        let fresh_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("fresh")]);
        let stale_presentation = PresentationFile::default();
        let fresh_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "concurrent presentation".into(),
            }],
        };
        let stale_meta = SessionMeta::new(id, "/p", 1);
        manager.save_snapshot(id, &stale_snapshot).unwrap();
        manager.write_presentation(id, &stale_presentation).unwrap();
        manager.write_meta(&stale_meta).unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        manager.rename(id, "concurrent user title").unwrap();
        manager.save_snapshot(id, &fresh_snapshot).unwrap();
        manager.write_presentation(id, &fresh_presentation).unwrap();

        let adopted = adopt_unconfirmed_native(
            &manager,
            &lease,
            stale_meta,
            Some(stale_snapshot),
            Some(stale_presentation),
        )
        .unwrap();

        assert_eq!(adopted.status, ImportStatus::AdoptedNative);
        assert_eq!(adopted.meta.owner, StorageOwner::Native);
        assert_eq!(adopted.meta.name, "concurrent user title");
        assert!(adopted.meta.user_renamed);
        assert_eq!(adopted.snapshot, fresh_snapshot);
        assert_eq!(adopted.presentation, fresh_presentation);
        let stored = manager.load_native_session(id).unwrap();
        assert_eq!(stored.meta, adopted.meta);
        assert_eq!(stored.snapshot, adopted.snapshot);
        assert_eq!(stored.presentation, adopted.presentation);
    }

    #[test]
    fn native_rename_preserves_user_priority_over_ai_naming() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "native-rename";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        manager.write_meta(&meta).unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: meta.name.clone(),
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: "/project".into(),
            created_at_ms: meta.created_at,
            updated_at_ms: meta.updated_at,
            message_count: 0,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        };

        let old = rename_catalog_entry_in_root(dir.path(), &entry, "chosen", false).unwrap();
        assert_eq!(old.as_deref(), Some(entry.name.as_str()));
        assert!(rename_catalog_entry_in_root(dir.path(), &entry, "ai", true)
            .unwrap()
            .is_none());
        let renamed = manager.read_meta(id).unwrap();
        assert_eq!(renamed.name, "chosen");
        assert!(renamed.user_renamed);
        assert!(!renamed.ai_named);
    }

    #[test]
    fn project_scoped_rename_ignores_duplicate_id_in_other_bucket() {
        use atomcode_capabilities::session::{PresentationFile, SessionManager, SessionMeta};

        let dir = tempfile::tempdir().unwrap();
        let id = "duplicate-id";
        let first_bucket = "1111111111111111";
        let second_bucket = "2222222222222222";
        for (bucket, name) in [(first_bucket, "first"), (second_bucket, "second")] {
            let manager = SessionManager::with_root(dir.path().join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot =
                atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(name)]);
            let mut meta = SessionMeta::new(id, format!("/{name}"), 1);
            meta.name = name.into();
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        }

        let old =
            rename_catalog_session_in_project_root(dir.path(), first_bucket, id, "chosen", false)
                .unwrap();

        assert_eq!(old.as_deref(), Some("first"));
        assert_eq!(
            SessionManager::with_root(dir.path().join(first_bucket))
                .read_meta(id)
                .unwrap()
                .name,
            "chosen"
        );
        assert_eq!(
            SessionManager::with_root(dir.path().join(second_bucket))
                .read_meta(id)
                .unwrap()
                .name,
            "second"
        );

        let applied =
            rename_catalog_session_in_project_root(dir.path(), second_bucket, id, "ai name", true)
                .unwrap();
        assert_eq!(applied.as_deref(), Some("second"));
        let second = SessionManager::with_root(dir.path().join(second_bucket))
            .read_meta(id)
            .unwrap();
        assert_eq!(second.name, "ai name");
        assert!(second.ai_named);
        assert!(!second.user_renamed);
    }

    #[test]
    fn catalog_session_view_uses_strict_native_aggregate() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "native-view";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        let presentation = PresentationFile::default();
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.name = "named native session".into();
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: meta.name.clone(),
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: "/project".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            message_count: 1,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        };

        let loaded = load_catalog_session_view_in_root(dir.path(), &entry).unwrap();
        assert_eq!(loaded.meta, meta);
        assert_eq!(loaded.snapshot, snapshot);
        assert_eq!(loaded.presentation, presentation);

        std::fs::remove_file(manager.presentation_path(id).unwrap()).unwrap();
        assert!(load_catalog_session_view_in_root(dir.path(), &entry).is_err());
    }

    #[test]
    fn catalog_preview_derives_from_snapshot_for_a_pre_owner_native_session() {
        // Pre-owner native session: valid meta (owner=Unconfirmed) + snapshot but
        // NO presentation sidecar — the state of the entire not-yet-resumed
        // backlog. Read-only, it must STILL preview (derive from the snapshot),
        // not report "unavailable"/corrupt.
        let root = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "preview-preowner";
        let manager = SessionManager::with_root(root.path().join(bucket));

        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
            KernelMessage::user("please refactor the auth module"),
            KernelMessage::assistant(
                "Sure — updating it now.\nsecond line of the same message",
                vec![],
            ),
        ]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Unconfirmed;
        meta.message_count = 2;
        manager.write_meta(&meta).unwrap();
        std::fs::write(
            manager.snapshot_path(id).unwrap(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        let preview = preview_catalog_session_in_project_root(root.path(), bucket, id)
            .unwrap()
            .expect("a pre-owner native session must still preview, not be 'unavailable'");

        assert_eq!(preview.session_id, id);
        // Derived from the snapshot's user/assistant messages; the multi-line
        // assistant reply collapses to ONE line and the user prompt is kept (S4).
        assert_eq!(
            preview.excerpt,
            vec![
                "please refactor the auth module".to_string(),
                "Sure — updating it now.".to_string(),
            ]
        );
    }

    #[test]
    fn catalog_preview_derives_from_legacy_blob_read_only() {
        // Legacy-only session: no native meta, data in a single legacy JSON blob.
        // The preview must convert it in memory (no persist / no ownership) and
        // still project a preview.
        let root = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let legacy = full_legacy_session();
        let id = legacy.id.clone();
        let manager = SessionManager::with_root(root.path().join(bucket));
        let legacy_path = manager.legacy_path(&id).unwrap();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let preview = preview_catalog_session_in_project_root(root.path(), bucket, &id)
            .unwrap()
            .expect("a legacy-only session must still preview");

        assert_eq!(preview.session_id, id);
        assert!(
            !preview.excerpt.is_empty(),
            "legacy preview must derive an excerpt from the converted presentation"
        );
        // Nothing was persisted: the read-only preview must not create native
        // meta / snapshot / presentation sidecars.
        assert!(!manager.meta_path(&id).unwrap().exists());
    }

    #[test]
    fn catalog_preview_keeps_last_six_messages_not_lines() {
        // S4: a single multi-line assistant message must not fill the excerpt and
        // evict earlier messages — each message contributes exactly one line.
        let root = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "preview-sixmsgs";
        let manager = SessionManager::with_root(root.path().join(bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
            "anchor",
        )]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        // 8 single-line user messages then one 20-line assistant message.
        let mut entries: Vec<PresentationEntry> = (0..8)
            .map(|n| PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::User,
                text: format!("msg-{n}"),
            })
            .collect();
        entries.push(PresentationEntry {
            anchor: DisplayAnchor::AtStart,
            role: PresentationRole::Assistant,
            text: (0..20)
                .map(|n| format!("reply line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries,
        };
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        drop(lease);

        let preview = preview_catalog_session_in_project_root(root.path(), bucket, id)
            .unwrap()
            .unwrap();

        // 6 messages: msg-3..msg-7 (5 users) + the assistant's FIRST line only.
        assert_eq!(preview.excerpt.len(), 6);
        assert_eq!(preview.excerpt.first().map(String::as_str), Some("msg-3"));
        assert_eq!(
            preview.excerpt.last().map(String::as_str),
            Some("reply line 0"),
            "a multi-line message contributes ONE line, not many"
        );
    }

    #[test]
    fn catalog_preview_is_bounded_sanitized_and_does_not_load_snapshot() {
        use atomcode_capabilities::session::{ModelUsageStat, TokenBreakdown};

        let root = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "preview-native";
        let manager = SessionManager::with_root(root.path().join(bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
            KernelMessage::user("snapshot must not be loaded"),
        ]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        for (turn_id, usage) in [
            (1, None),
            (2, Some(("provider-old", "model-old"))),
            (3, None),
            (4, Some(("provider-new", "model-new"))),
        ] {
            meta.turn_stats.push(TurnStat {
                after_message: 1,
                position_valid: true,
                turn_id,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 128,
                model_usage: usage
                    .map(|(provider_id, model_id)| {
                        vec![ModelUsageStat {
                            provider_id: provider_id.into(),
                            model_id: model_id.into(),
                            tokens: TokenBreakdown {
                                input: 1,
                                output: 1,
                                cached_input: 0,
                            },
                        }]
                    })
                    .unwrap_or_default(),
            });
        }
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: (0..10)
                .map(|line| PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: if line == 9 {
                        "tail\u{1b}[31m\u{7}".into()
                    } else {
                        format!("line-{line}")
                    },
                })
                .collect(),
        };
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        drop(lease);

        // Poison the snapshot after the aggregate is committed. Preview is a
        // meta + presentation projection and must remain usable.
        std::fs::write(manager.snapshot_path(id).unwrap(), b"not-json").unwrap();
        let preview = preview_catalog_session_in_project_root(root.path(), bucket, id)
            .unwrap()
            .unwrap();

        assert_eq!(preview.session_id, id);
        assert_eq!(preview.project_bucket, bucket);
        assert_eq!(preview.provider_id.as_deref(), Some("provider-new"));
        assert_eq!(preview.model_id.as_deref(), Some("model-new"));
        assert_eq!(preview.excerpt.len(), 6);
        assert_eq!(
            preview.excerpt.first().map(String::as_str),
            Some("line-4")
        );
        assert_eq!(preview.excerpt.last().map(String::as_str), Some("tail"));
        assert!(preview.excerpt.iter().all(|line| {
            !line.chars().any(char::is_control) && !line.contains("[31m")
        }));
    }

    #[test]
    fn catalog_preview_returns_typed_sanitized_corrupt_and_oversize_errors() {
        let root = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "preview-errors";
        let manager = SessionManager::with_root(root.path().join(bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &lease,
                Some(&atomcode_kernel::message::SessionSnapshot::new(Vec::new())),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        drop(lease);
        let presentation_path = manager.presentation_path(id).unwrap();

        std::fs::write(&presentation_path, b"{not-json").unwrap();
        let corrupt = preview_catalog_session_in_project_root(root.path(), bucket, id).unwrap_err();
        assert_eq!(corrupt, CatalogSessionPreviewError::Corrupt);
        assert_eq!(corrupt.to_string(), "session preview data is corrupt");
        assert!(!corrupt.to_string().contains(id));
        assert!(!corrupt
            .to_string()
            .contains(&root.path().to_string_lossy().to_string()));

        let file = std::fs::File::create(&presentation_path).unwrap();
        file.set_len(
            u64::try_from(
                atomcode_capabilities::session::presentation::MAX_PRESENTATION_BYTES + 1,
            )
            .unwrap(),
        )
        .unwrap();
        let oversized =
            preview_catalog_session_in_project_root(root.path(), bucket, id).unwrap_err();
        assert_eq!(oversized, CatalogSessionPreviewError::TooLarge);
        assert_eq!(oversized.to_string(), "session preview data is too large");
    }

    #[test]
    fn catalog_session_view_repairs_complete_native_placeholder_name() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "native-placeholder";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
            "恢复自动命名",
        )]);
        let presentation = PresentationFile::default();
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        drop(lease);
        let entry = CatalogEntry {
            id: id.into(),
            name: meta.name,
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: "/project".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            message_count: 1,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        };

        let loaded = load_catalog_session_view_in_root(dir.path(), &entry).unwrap();

        assert_eq!(loaded.meta.name, "恢复自动命名");
        assert_eq!(manager.read_meta(id).unwrap().name, "恢复自动命名");
    }

    #[test]
    fn project_catalog_repairs_placeholder_before_first_resume_list() {
        let dir = tempfile::tempdir().unwrap();
        let working_dir = std::path::Path::new("/project");
        let bucket = SessionManager::project_hash(working_dir);
        let id = "catalog-placeholder";
        let manager = SessionManager::with_root(dir.path().join(&bucket));
        let lease = manager.acquire_lease(id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
            "首次展示名称",
        )]);
        let mut meta = SessionMeta::new(id, working_dir.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        drop(lease);

        let entries = catalog_for_project_in_root(dir.path(), working_dir).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "首次展示名称");
        assert_eq!(manager.read_meta(id).unwrap().name, "首次展示名称");
    }

    #[test]
    fn project_catalog_includes_equivalent_working_dir_from_historical_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let working_dir = dir.path().join("project");
        std::fs::create_dir_all(&working_dir).unwrap();
        let current_bucket = SessionManager::project_hash(&working_dir);
        let historical_bucket = "1111111111111111";

        for (bucket, id, stored_working_dir) in [
            (current_bucket.as_str(), "current", working_dir.clone()),
            (historical_bucket, "historical", working_dir.clone()),
        ] {
            let manager = SessionManager::with_root(dir.path().join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot =
                atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(id)]);
            let mut meta = SessionMeta::new(id, stored_working_dir.to_string_lossy(), 1);
            meta.name = id.into();
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        }

        let entries = catalog_for_project_in_root(dir.path(), &working_dir).unwrap();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| {
            entry.id == "historical" && entry.project_bucket == historical_bucket
        }));
    }

    #[test]
    fn project_catalog_excludes_other_working_dir_from_historical_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let working_dir = dir.path().join("project");
        let other_dir = dir.path().join("other");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        let historical_bucket = "2222222222222222";
        let manager = SessionManager::with_root(dir.path().join(historical_bucket));
        let lease = manager.acquire_lease("other").unwrap();
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("other")]);
        let mut meta = SessionMeta::new("other", other_dir.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();

        let entries = catalog_for_project_in_root(dir.path(), &working_dir).unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn project_catalog_name_repair_failure_does_not_hide_healthy_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let working_dir = std::path::Path::new("/project");
        let bucket = SessionManager::project_hash(working_dir);
        let manager = SessionManager::with_root(dir.path().join(&bucket));

        let healthy_id = "healthy-placeholder";
        let healthy_lease = manager.acquire_lease(healthy_id).unwrap();
        let healthy_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("健康会话")]);
        let mut healthy_meta = SessionMeta::new(healthy_id, working_dir.to_string_lossy(), 2);
        healthy_meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &healthy_lease,
                Some(&healthy_snapshot),
                Some(&PresentationFile::default()),
                &healthy_meta,
            )
            .unwrap();
        drop(healthy_lease);

        let damaged_id = "damaged-placeholder";
        let damaged_snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("损坏会话")]);
        manager
            .save_snapshot(damaged_id, &damaged_snapshot)
            .unwrap();
        let mut damaged_meta = SessionMeta::new(damaged_id, working_dir.to_string_lossy(), 1);
        damaged_meta.owner = StorageOwner::Native;
        manager.write_meta(&damaged_meta).unwrap();

        let entries = catalog_for_project_in_root(dir.path(), working_dir).unwrap();

        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == healthy_id)
                .map(|entry| entry.name.as_str()),
            Some("健康会话")
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == damaged_id)
                .map(|entry| entry.name.as_str()),
            Some("session-damaged-placeholder")
        );
    }

    #[test]
    fn catalog_session_view_adopts_pre_owner_native_without_presentation() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "pre-owner-native";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let snapshot =
            atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user("native")]);
        let meta = SessionMeta::new(id, "/project", 1);
        manager.save_snapshot(id, &snapshot).unwrap();
        manager.write_meta(&meta).unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: meta.name.clone(),
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: "/project".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            message_count: 1,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        };

        let loaded = load_catalog_session_view_in_root(dir.path(), &entry).unwrap();

        assert_eq!(loaded.meta.owner, StorageOwner::Native);
        assert_eq!(loaded.meta.name, "native");
        assert_eq!(loaded.snapshot, snapshot);
        assert_eq!(loaded.presentation, PresentationFile::default());
        assert_eq!(manager.read_meta(id).unwrap().owner, StorageOwner::Native);
        assert_eq!(
            manager.read_presentation(id).unwrap(),
            PresentationFile::default()
        );
    }

    #[test]
    fn legacy_catalog_session_view_is_read_only() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let project = dir.path().join(bucket);
        std::fs::create_dir_all(&project).unwrap();
        let session = full_legacy_session();
        let id = session.id.as_str();
        std::fs::write(
            project.join(format!("{id}.json")),
            include_bytes!("../tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: session.name.clone(),
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: session.working_dir.clone(),
            created_at_ms: session.created_at as i64 * 1_000,
            updated_at_ms: session.updated_at as i64 * 1_000,
            message_count: session.messages.len(),
            turn_count: session.turn_stats.len(),
            presence: CatalogPresence::LegacyOnly,
        };

        let loaded = load_catalog_session_view_in_root(dir.path(), &entry).unwrap();
        assert_eq!(loaded.meta.owner, StorageOwner::Legacy);
        assert_eq!(
            loaded.snapshot.messages.len(),
            session.messages.len() + session.cold_summaries.len()
        );
        assert!(!project.join(format!("{id}.meta")).exists());
        assert!(!project.join(format!("{id}.snapshot")).exists());
    }

    #[test]
    fn legacy_rename_cuts_over_once_then_writes_only_native_meta() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let project = dir.path().join(bucket);
        std::fs::create_dir_all(&project).unwrap();
        let session = full_legacy_session();
        let id = session.id.as_str();
        let legacy_bytes = include_bytes!("../tests/fixtures/session/legacy_full.json");
        std::fs::write(project.join(format!("{id}.json")), legacy_bytes).unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: session.name.clone(),
            fork_root_id: None,
            project_bucket: bucket.into(),
            working_dir: session.working_dir.clone(),
            created_at_ms: session.created_at as i64 * 1_000,
            updated_at_ms: session.updated_at as i64 * 1_000,
            message_count: session.messages.len(),
            turn_count: session.turn_stats.len(),
            presence: CatalogPresence::LegacyOnly,
        };

        rename_catalog_entry_in_root(dir.path(), &entry, "native-name", false).unwrap();

        let manager = SessionManager::with_root(&project);
        let meta = manager.read_meta(id).unwrap();
        assert_eq!(meta.owner, StorageOwner::Native);
        assert_eq!(meta.name, "native-name");
        assert!(meta.user_renamed);
        assert_eq!(
            std::fs::read(project.join(format!("{id}.json"))).unwrap(),
            legacy_bytes
        );
    }

    #[test]
    fn delete_uses_active_lease_cleans_every_artifact_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "delete-all";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        std::fs::create_dir_all(manager.root()).unwrap();
        for path in [
            manager.snapshot_path(id).unwrap(),
            manager.meta_path(id).unwrap(),
            manager.jsonl_path(id).unwrap(),
            manager.presentation_path(id).unwrap(),
            manager.legacy_path(id).unwrap(),
        ] {
            std::fs::write(path, b"fixture").unwrap();
        }

        let active = manager.acquire_lease(id).unwrap();
        let error = delete_catalog_session_in_root(dir.path(), bucket, id).unwrap_err();
        assert!(error.to_string().contains("already in use"), "{error:#}");
        assert!(manager.legacy_path(id).unwrap().exists());
        drop(active);

        delete_catalog_session_in_root(dir.path(), bucket, id).unwrap();
        for path in [
            manager.snapshot_path(id).unwrap(),
            manager.meta_path(id).unwrap(),
            manager.jsonl_path(id).unwrap(),
            manager.presentation_path(id).unwrap(),
            manager.legacy_path(id).unwrap(),
        ] {
            assert!(!path.exists(), "{} was not deleted", path.display());
        }
        let missing = delete_catalog_session_in_root(dir.path(), bucket, id).unwrap_err();
        assert!(matches!(
            missing.downcast_ref::<SessionStoreError>(),
            Some(SessionStoreError::NotFound { .. })
        ));
    }

    #[test]
    fn delete_rejects_untrusted_project_bucket_before_path_join() {
        let dir = tempfile::tempdir().unwrap();
        let error = delete_catalog_session_in_root(dir.path(), "../outside", "id").unwrap_err();
        assert!(error.to_string().contains("invalid project bucket"));
    }

    #[test]
    fn native_ui_append_uses_stable_turn_anchor_and_not_runtime_snapshot() {
        use atomcode_capabilities::session::{DisplayAnchor, TurnStat};

        let dir = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let id = "native-ui";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![KernelMessage::user(
            "runtime-only",
        )]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        meta.turn_stats.push(TurnStat {
            after_message: 1,
            position_valid: true,
            turn_id: 7,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
        });
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();

        let count = append_catalog_presentation_in_root(
            dir.path(),
            bucket,
            id,
            &[(PresentationRole::Assistant, "local note".into())],
        )
        .unwrap();

        assert_eq!(count, 2);
        assert_eq!(manager.load_snapshot(id).unwrap(), snapshot);
        let presentation = manager.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 1);
        assert_eq!(
            presentation.entries[0].anchor,
            DisplayAnchor::AfterTurn { turn_id: 7 }
        );
        assert_eq!(presentation.entries[0].text, "local note");
    }
}
