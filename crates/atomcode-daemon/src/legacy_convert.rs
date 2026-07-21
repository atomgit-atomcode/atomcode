use atomcode_core::conversation::message::{
    ImagePart, Message as CoreMessage, MessageContent, Role as CoreRole,
};
use atomcode_core::conversation::{
    ConversationSnapshot, LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
};
use atomcode_kernel::message::{ImageContent, Message as KernelMessage, Role as KernelRole};
use serde::{Deserialize, Serialize};

use atomcode_capabilities::session::manager::META_VERSION;
use atomcode_capabilities::session::presentation::PRESENTATION_VERSION;
use atomcode_capabilities::session::{
    anchor_from_legacy_position, DisplayAnchor, ImportInfo, ImportKind, LegacyTurnBoundary,
    PresentationEntry, PresentationFile, PresentationRole, SessionLease, SessionManager,
    SessionMeta, SessionResult, StorageOwner, TurnStat,
};

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

pub const IMPORTER_VERSION: u32 = 1;
pub const LEGACY_SCHEMA: &str = "core-session-json";

/// Frozen reader for the retired core session JSON schema. Keeping this DTO
/// private prevents legacy persistence fields from leaking back into drivers.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySession {
    id: String,
    name: String,
    working_dir: std::path::PathBuf,
    created_at: u64,
    updated_at: u64,
    messages: Vec<CoreMessage>,
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

impl LegacySession {
    fn to_conversation_snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            messages: self.messages.clone(),
            cold_summaries: self.cold_summaries.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyDisplayMessage {
    after_message: usize,
    message: CoreMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTurnStat {
    after_message: usize,
    turn_count: usize,
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
    RepairedLegacyTurnBoundaries { dropped_turn_stats: usize },
}

fn report_import_diagnostic(session_id: &str, diagnostic: Option<ImportDiagnostic>) {
    if let Some(ImportDiagnostic::RepairedLegacyTurnBoundaries { dropped_turn_stats }) = diagnostic
    {
        tracing::warn!(
            session_id,
            dropped_turn_stats,
            "repaired malformed legacy turn boundaries during import"
        );
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

pub fn image_to_kernel(image: &ImagePart) -> ImageContent {
    ImageContent {
        media_type: image.media_type.clone(),
        data: image.data.clone(),
    }
}

fn role_to_kernel(role: &CoreRole) -> KernelRole {
    match role {
        CoreRole::System => KernelRole::System,
        CoreRole::User => KernelRole::User,
        CoreRole::Assistant => KernelRole::Assistant,
        CoreRole::Tool => KernelRole::Tool,
    }
}

fn role_to_core(role: &KernelRole) -> CoreRole {
    match role {
        KernelRole::System => CoreRole::System,
        KernelRole::User => CoreRole::User,
        KernelRole::Assistant => CoreRole::Assistant,
        KernelRole::Tool => CoreRole::Tool,
    }
}

pub fn message_to_kernel(message: &CoreMessage) -> KernelMessage {
    let mut converted = match &message.content {
        MessageContent::Text(text) => {
            let mut converted = KernelMessage::user(text.clone());
            converted.role = role_to_kernel(&message.role);
            converted
        }
        MessageContent::AssistantWithToolCalls {
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
        MessageContent::ToolResult(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.output.clone(),
            !result.success,
        ),
        MessageContent::ToolResultRef(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.summary.clone(),
            !result.success,
        ),
        MessageContent::MultiPart { text, images } => KernelMessage::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(image_to_kernel).collect(),
        ),
    };
    converted.synthetic = message.synthetic;
    converted.internal_origin = message.internal_origin.clone();
    converted
}

pub(crate) fn message_to_core(message: &KernelMessage) -> CoreMessage {
    let content = if message.role == KernelRole::Tool {
        MessageContent::ToolResult(atomcode_core::tool::ToolResult {
            call_id: message.tool_call_id.clone().unwrap_or_default(),
            output: message.text.clone(),
            success: !message.is_error,
        })
    } else if !message.tool_calls.is_empty() {
        MessageContent::AssistantWithToolCalls {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| atomcode_core::tool::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
            reasoning_content: message.reasoning.clone(),
            thinking_blocks: message
                .reasoning_blocks
                .iter()
                .map(
                    |block| atomcode_core::conversation::message::ThinkingBlock {
                        text: block.text.clone(),
                        signature: block.opaque.clone().unwrap_or_default(),
                    },
                )
                .collect(),
        }
    } else if !message.images.is_empty() {
        MessageContent::MultiPart {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            images: message
                .images
                .iter()
                .map(|image| ImagePart {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect(),
        }
    } else {
        MessageContent::Text(message.text.clone())
    };
    CoreMessage {
        role: role_to_core(&message.role),
        content,
        synthetic: message.synthetic,
        internal_origin: message.internal_origin.clone(),
    }
}

pub fn snapshot_to_kernel(
    snapshot: &ConversationSnapshot,
) -> atomcode_kernel::message::SessionSnapshot {
    let mut messages = Vec::with_capacity(snapshot.messages.len() + snapshot.cold_summaries.len());
    for summary in &snapshot.cold_summaries {
        let mut message = KernelMessage::user(format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}"));
        message.synthetic = true;
        message.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        messages.push(message);
    }
    messages.extend(snapshot.messages.iter().map(message_to_kernel));
    atomcode_kernel::message::SessionSnapshot::new(messages)
}

struct NormalizedLegacyTurns {
    boundaries: Vec<LegacyTurnBoundary>,
    turn_stats: Vec<TurnStat>,
    dropped_turn_stats: usize,
}

fn normalize_legacy_turns(session: &LegacySession) -> anyhow::Result<NormalizedLegacyTurns> {
    let mut boundaries = Vec::with_capacity(session.turn_stats.len());
    let mut turn_stats = Vec::with_capacity(session.turn_stats.len());
    let mut previous_after = 0usize;
    let mut dropped_turn_stats = 0usize;

    for stat in &session.turn_stats {
        if stat.after_message > session.messages.len() {
            anyhow::bail!("legacy turn boundary is outside the message history")
        }
        if stat.after_message == 0 || stat.after_message <= previous_after {
            dropped_turn_stats += 1;
            continue;
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
            turn_id,
            round_count: checked_u32(stat.turn_count, "turn_count")?,
            tool_call_count: checked_u32(stat.tool_call_count, "tool_call_count")?,
            duration_ms: stat.duration_ms,
            total_tokens: checked_u32(stat.total_tokens, "total_tokens")?,
            errored: stat.errored,
            used_tokens: checked_u32(stat.used_tokens, "used_tokens")?,
            ctx_window: checked_u32(stat.ctx_window, "ctx_window")?,
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
                let role = match display.message.role {
                    CoreRole::User => PresentationRole::User,
                    CoreRole::Assistant => PresentationRole::Assistant,
                    ref role => {
                        anyhow::bail!(
                            "legacy presentation role {role:?} is not supported by schema v1"
                        )
                    }
                };
                let MessageContent::Text(text) = &display.message.content else {
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

    let mut snapshot = snapshot_to_kernel(&session.to_conversation_snapshot());
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
        working_dir,
        created_at,
        updated_at,
        turn_count: checked_u32(normalized_turns.turn_stats.len(), "turn_stats")?,
        message_count: checked_u32(session.messages.len(), "messages")?,
        turn_stats: normalized_turns.turn_stats,
    };
    meta.auto_name_from_messages(&snapshot.messages);

    let diagnostic = (normalized_turns.dropped_turn_stats > 0).then_some(
        ImportDiagnostic::RepairedLegacyTurnBoundaries {
            dropped_turn_stats: normalized_turns.dropped_turn_stats,
        },
    );

    Ok((
        ConvertedLegacySession {
            snapshot,
            meta,
            presentation,
        },
        diagnostic,
    ))
}

/// Resolve a session's storage state and, when needed, publish native ownership.
/// The caller must hold the exact bucket/session lease for the whole operation.
pub fn converge_session(
    manager: &SessionManager,
    lease: &SessionLease,
) -> anyhow::Result<ImportOutcome> {
    use sha2::{Digest, Sha256};

    let id = lease.id();
    let existing_meta = optional_store(manager.read_meta(id))?;
    let existing_snapshot = optional_store(manager.load_snapshot(id))?;
    let existing_presentation = optional_store(manager.read_presentation(id))?;
    let legacy_bytes = optional_store(manager.read_legacy_bytes(id))?;

    if existing_meta.as_ref().map(|meta| &meta.owner) == Some(&StorageOwner::Native) {
        let meta = existing_meta.expect("checked above");
        let snapshot = existing_snapshot.ok_or_else(|| {
            anyhow::anyhow!("owner=native session {id:?} is missing its snapshot")
        })?;
        let presentation = existing_presentation.ok_or_else(|| {
            anyhow::anyhow!("owner=native session {id:?} is missing presentation")
        })?;
        let diagnostic = match (legacy_bytes.as_deref(), meta.import_info.as_ref()) {
            (Some(bytes), Some(info)) if sha256_hex(bytes) != info.source_sha256 => {
                Some(ImportDiagnostic::LegacyChangedAfterCutover)
            }
            _ => None,
        };
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
    let Some(legacy_bytes) = legacy_bytes else {
        return match (existing_meta, existing_snapshot) {
            (Some(mut meta), Some(snapshot)) if meta.owner == StorageOwner::Unconfirmed => {
                meta.auto_name_from_messages(&snapshot.messages);
                meta.owner = StorageOwner::Native;
                meta.import_info = None;
                let write_presentation = existing_presentation.is_none();
                let presentation = existing_presentation.unwrap_or_default();
                manager.commit_native_import(
                    lease,
                    None,
                    write_presentation.then_some(&presentation),
                    &meta,
                )?;
                Ok(ImportOutcome {
                    status: ImportStatus::AdoptedNative,
                    diagnostic: None,
                    snapshot,
                    meta,
                    presentation,
                })
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
    let (mut converted, diagnostic) = convert_legacy_session_with_diagnostic(&legacy)?;
    let preserve_native_snapshot = existing_snapshot.is_some() && !force_legacy;
    if preserve_native_snapshot {
        let base = existing_snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.turn_counter);
        rebase_converted_turn_ids(&mut converted, base)?;
    }
    let mut snapshot = if preserve_native_snapshot {
        existing_snapshot.expect("checked above")
    } else {
        converted.snapshot.clone()
    };
    let kind = if preserve_native_snapshot {
        ImportKind::MetadataOnly
    } else {
        ImportKind::Full
    };
    let mut meta = if preserve_native_snapshot {
        existing_meta.unwrap_or_else(|| converted.meta.clone())
    } else {
        converted.meta.clone()
    };
    meta.auto_name_from_messages(&snapshot.messages);
    if preserve_native_snapshot
        && (meta.turn_stats.is_empty() || meta.turn_stats.iter().any(|stat| stat.turn_id == 0))
    {
        meta.turn_stats = converted.meta.turn_stats.clone();
        meta.turn_count = converted.meta.turn_count;
    }
    let previous_turn_counter = snapshot.turn_counter;
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
        source_sha256: format!("{:x}", Sha256::digest(&legacy_bytes)),
        importer_version: IMPORTER_VERSION,
        kind: kind.clone(),
    });

    let preserve_presentation = existing_presentation.is_some() && !force_legacy;
    let presentation = if preserve_presentation {
        existing_presentation.expect("checked above")
    } else {
        converted.presentation
    };
    manager.commit_native_import(
        lease,
        (!preserve_native_snapshot || snapshot.turn_counter != previous_turn_counter)
            .then_some(&snapshot),
        (!preserve_presentation).then_some(&presentation),
        &meta,
    )?;
    report_import_diagnostic(id, diagnostic);

    Ok(ImportOutcome {
        status: if preserve_native_snapshot {
            ImportStatus::ImportedMetadataOnly
        } else {
            ImportStatus::ImportedFull
        },
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
        .filter(|entry| entry.project_bucket == bucket)
        .collect();
    for entry in &mut entries {
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
    Ok(entries)
}

pub fn find_catalog_session_view(query: &str) -> anyhow::Result<Option<CatalogSessionView>> {
    let scan = SessionManager::scan_all();
    report_catalog_diagnostics(&scan.diagnostics);
    let entry = scan.find(query)?;
    if entry.is_none() {
        reject_matching_catalog_diagnostic(&scan.diagnostics, query)?;
    }
    entry.as_ref().map(load_catalog_session_view).transpose()
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

pub fn rename_catalog_session(query: &str, new_name: &str) -> anyhow::Result<String> {
    rename_catalog_session_inner(query, new_name, false)?
        .ok_or_else(|| anyhow::anyhow!("session {query:?} rejected a user rename unexpectedly"))
}

pub fn apply_ai_catalog_name(query: &str, new_name: &str) -> anyhow::Result<bool> {
    Ok(rename_catalog_session_inner(query, new_name, true)?.is_some())
}

pub fn rename_catalog_session_in_project(
    project_bucket: &str,
    id: &str,
    new_name: &str,
) -> anyhow::Result<String> {
    let scan = SessionManager::scan_all();
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
    rename_catalog_entry_inner(entry, new_name, false)?.ok_or_else(|| {
        anyhow::anyhow!("session {project_bucket}/{id} rejected a user rename unexpectedly")
    })
}

/// Delete every persisted representation of a session under the same active-session
/// lease used by native runtimes. The project bucket is an external API value, so it
/// must be validated before it is joined below the sessions root.
pub fn delete_catalog_session_in_project(project_bucket: &str, id: &str) -> anyhow::Result<()> {
    delete_catalog_session_in_root(&SessionManager::sessions_root(), project_bucket, id)
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
    snapshot: &ConversationSnapshot,
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
    let mut native_snapshot = snapshot_to_kernel(snapshot);
    if has_existing {
        let outcome = converge_session(&manager, &lease)?;
        native_snapshot.turn_counter = native_snapshot
            .turn_counter
            .max(outcome.snapshot.turn_counter);
        native_snapshot.request_counter = native_snapshot
            .request_counter
            .max(outcome.snapshot.request_counter);
        let mut meta = outcome.meta;
        meta.message_count = u32::try_from(native_snapshot.messages.len())?;
        meta.updated_at = atomcode_capabilities::session::now_ms();
        manager.commit_native_runtime_mutation(
            &lease,
            &native_snapshot,
            &outcome.presentation,
            &meta,
        )?;
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
    let meta = match meta {
        Some(meta) if meta.owner == StorageOwner::Native || legacy.is_none() => meta,
        _ if legacy.is_some() => {
            let lease = manager.acquire_lease(id)?;
            let outcome = converge_session(&manager, &lease)?;
            cutover_lease = Some(lease);
            outcome.meta
        }
        _ => anyhow::bail!("session {project_bucket}/{id} not found"),
    };
    let anchor = meta
        .turn_stats
        .last()
        .map(
            |stat| atomcode_capabilities::session::DisplayAnchor::AfterTurn {
                turn_id: stat.turn_id,
            },
        )
        .unwrap_or(atomcode_capabilities::session::DisplayAnchor::AtStart);
    let mut presentation = optional_store(manager.read_presentation(id))?.unwrap_or_default();
    presentation
        .entries
        .extend(messages.iter().map(|(role, text)| PresentationEntry {
            anchor: anchor.clone(),
            role: role.clone(),
            text: text.clone(),
        }));
    manager.write_presentation(id, &presentation)?;
    drop(cutover_lease);
    Ok(meta.message_count as usize + presentation.entries.len())
}

fn delete_catalog_session_in_root(
    sessions_root: &std::path::Path,
    project_bucket: &str,
    id: &str,
) -> anyhow::Result<()> {
    validate_project_bucket(project_bucket)?;
    let manager = SessionManager::with_root(sessions_root.join(project_bucket));
    let lease = manager.acquire_lease(id)?;
    manager.delete(&lease)?;
    Ok(())
}

fn validate_project_bucket(project_bucket: &str) -> anyhow::Result<()> {
    if project_bucket.len() != 16 || !project_bucket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid project bucket {project_bucket:?}")
    }
    Ok(())
}

fn rename_catalog_session_inner(
    query: &str,
    new_name: &str,
    ai: bool,
) -> anyhow::Result<Option<String>> {
    let scan = SessionManager::scan_all();
    report_catalog_diagnostics(&scan.diagnostics);
    let entry = scan.find(query)?.ok_or_else(|| {
        reject_matching_catalog_diagnostic(&scan.diagnostics, query)
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("session {query:?} not found"))
    })?;
    rename_catalog_entry_inner(&entry, new_name, ai)
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

fn rename_catalog_entry_inner(
    entry: &atomcode_capabilities::session::CatalogEntry,
    new_name: &str,
    ai: bool,
) -> anyhow::Result<Option<String>> {
    rename_catalog_entry_in_root(&SessionManager::sessions_root(), entry, new_name, ai)
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
    if ai
        && !atomcode_coding::session_title::should_accept_ai_name(meta.user_renamed, meta.ai_named)
    {
        return Ok(None);
    }
    let old_name = meta.name;
    if ai {
        manager.update_meta(&entry.id, |meta| {
            meta.name = new_name.to_string();
            meta.ai_named = true;
            meta.updated_at = atomcode_capabilities::session::now_ms();
        })?;
    } else {
        manager.rename(&entry.id, new_name)?;
    }
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

pub fn snapshot_to_core(
    snapshot: &atomcode_kernel::message::SessionSnapshot,
) -> ConversationSnapshot {
    let mut messages = Vec::with_capacity(snapshot.messages.len());
    let mut cold_summaries = Vec::new();
    for message in &snapshot.messages {
        if message.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN) {
            if let Some(summary) = message.text.strip_prefix(LEGACY_COLD_SUMMARY_PREFIX) {
                cold_summaries.push(summary.to_string());
                continue;
            }
        }
        messages.push(message_to_core(message));
    }
    ConversationSnapshot {
        messages,
        cold_summaries,
    }
}

pub fn usage_to_core(
    usage: &atomcode_kernel::stream::TokenUsage,
) -> atomcode_core::stream::TokenUsage {
    atomcode_core::stream::TokenUsage {
        prompt_tokens: usage.prompt as usize,
        completion_tokens: usage.completion as usize,
        cached_tokens: usage.cached as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_legacy_session() -> LegacySession {
        serde_json::from_str(include_str!(
            "../../atomcode-core/tests/fixtures/session/legacy_full.json"
        ))
        .expect("full legacy session fixture must parse")
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
    fn full_legacy_fixture_converts_to_expected_kernel_snapshot() {
        let session = full_legacy_session();
        let snapshot = snapshot_to_kernel(&session.to_conversation_snapshot());

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
            "../../atomcode-core/tests/fixtures/session/legacy_minimal.json"
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
        session.messages[3] = CoreMessage::new(CoreRole::User, "interrupt");

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
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json"),
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
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let imported = converge_session(&manager, &lease).unwrap();
        assert_eq!(imported.status, ImportStatus::ImportedMetadataOnly);
        assert_eq!(imported.snapshot.messages, native.messages);
        assert_eq!(imported.snapshot.cache_epoch, native.cache_epoch);
        assert_eq!(imported.snapshot.request_counter, native.request_counter);
        assert_eq!(imported.snapshot.turn_counter, 2);
        assert_eq!(manager.load_snapshot(id).unwrap(), imported.snapshot);
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
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json"),
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
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let lease = manager.acquire_lease(id).unwrap();

        let error = converge_session(&manager, &lease).unwrap_err();
        assert!(
            error.to_string().contains("missing its snapshot"),
            "{error:#}"
        );
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
        assert!(
            error.to_string().contains("missing presentation"),
            "{error:#}"
        );
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
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json"),
        )
        .unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: session.name.clone(),
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
        let legacy_bytes =
            include_bytes!("../../atomcode-core/tests/fixtures/session/legacy_full.json");
        std::fs::write(project.join(format!("{id}.json")), legacy_bytes).unwrap();
        let entry = CatalogEntry {
            id: id.into(),
            name: session.name.clone(),
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
    fn delete_uses_active_lease_and_cleans_every_session_artifact_idempotently() {
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
        manager.save_snapshot(id, &snapshot).unwrap();
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        meta.turn_stats.push(TurnStat {
            after_message: 1,
            turn_id: 7,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
        });
        manager.write_meta(&meta).unwrap();

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

    #[test]
    fn kernel_round_trip_characterizes_legacy_ref_summary_loss() {
        let session = full_legacy_session();
        let kernel = snapshot_to_kernel(&session.to_conversation_snapshot());
        let round_trip = snapshot_to_core(&kernel);

        assert_eq!(round_trip.cold_summaries, session.cold_summaries);
        assert_eq!(round_trip.messages.len(), session.messages.len());
        match &round_trip.messages[5].content {
            MessageContent::ToolResult(result) => {
                assert_eq!(result.call_id, "call-ref");
                assert_eq!(result.output, "cached failure summary");
                assert!(!result.success);
            }
            other => panic!("legacy ref currently returns as inline summary, got {other:?}"),
        }
    }
}
