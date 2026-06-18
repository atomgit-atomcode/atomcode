package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.daemon.DaemonSupervisorState
import com.atomcode.jetbrains.daemon.ModelInfo
import com.atomcode.jetbrains.daemon.ProviderInfo
import com.atomcode.jetbrains.daemon.SessionDetail
import com.atomcode.jetbrains.services.SessionRefView

data class ChatState(
    val tabId: String,
    val session: SessionRefView? = null,
    val connection: DaemonSupervisorState = DaemonSupervisorState.Idle,
    val generation: GenerationState = GenerationState.Idle,
    val messages: List<MessageState> = emptyList(),
    val queue: List<QueuedPromptState> = emptyList(),
    val pendingContext: List<ContextItemState> = emptyList(),
    val activePermission: PermissionRequestState? = null,
    val provider: ProviderSelectionState? = null,
    val draft: String = "",
)

sealed interface GenerationState {
    data object Idle : GenerationState
    data class Streaming(val assistantMessageId: String) : GenerationState
    data class WaitingPermission(val requestId: String) : GenerationState
    data object Stopping : GenerationState
    data class Failed(val message: String) : GenerationState
}

sealed interface MessageState {
    val id: String
    val createdAt: Long

    data class User(
        override val id: String,
        override val createdAt: Long,
        val text: String,
        val contextSummary: List<String>,
    ) : MessageState

    data class Assistant(
        override val id: String,
        override val createdAt: Long,
        val rawMarkdown: String,
        val status: AssistantStatus,
        val reasoning: String?,
    ) : MessageState

    data class ToolCall(
        override val id: String,
        override val createdAt: Long,
        val callId: String,
        val name: String,
        val argumentsJson: String,
        val outputChunks: List<String>,
        val status: ToolStatus,
        val durationMs: Long?,
    ) : MessageState

    data class Permission(
        override val id: String,
        override val createdAt: Long,
        val request: PermissionRequestState,
    ) : MessageState

    data class System(
        override val id: String,
        override val createdAt: Long,
        val text: String,
        val level: SystemLevel,
    ) : MessageState
}

enum class AssistantStatus {
    Streaming,
    Complete,
    Error,
}

enum class ToolStatus {
    Running,
    Success,
    Failed,
}

enum class SystemLevel {
    Info,
    Warning,
    Error,
}

data class QueuedPromptState(
    val id: String,
    val text: String,
    val contextSummary: List<String>,
    val createdAt: Long,
)

data class ContextItemState(
    val id: String,
    val path: String,
    val displayName: String,
    val language: String,
    val selectionStartLine: Int?,
    val selectionEndLine: Int?,
)

data class PermissionRequestState(
    val requestId: String,
    val sessionId: String,
    val toolName: String,
    val reason: String,
    val callId: String,
    val arguments: String,
)

enum class PermissionDecisionKind {
    AllowOnce,
    Deny,
    AlwaysAllow,
}

data class ProviderSelectionState(
    val provider: ProviderInfo,
    val model: ModelInfo?,
)

sealed interface ChatAction {
    data class SubmitPrompt(val text: String) : ChatAction
    data class QueuePrompt(val text: String, val id: String? = null) : ChatAction
    data class RemoveQueuedPrompt(val id: String) : ChatAction
    data class AddContext(val item: ContextItemState) : ChatAction
    data class RemoveContext(val id: String) : ChatAction
    data object ClearContext : ChatAction
    data class DraftChanged(val text: String) : ChatAction
    data class SessionRefUpdated(val session: SessionRefView) : ChatAction
    data class SessionLoaded(val detail: SessionDetail) : ChatAction
    data class ConnectionChanged(val state: DaemonSupervisorState) : ChatAction
    data class DaemonEventReceived(val event: com.atomcode.jetbrains.daemon.ChatEvent) : ChatAction
    data class PermissionDecision(val requestId: String, val decision: PermissionDecisionKind) : ChatAction
    data object StopRequested : ChatAction
}
