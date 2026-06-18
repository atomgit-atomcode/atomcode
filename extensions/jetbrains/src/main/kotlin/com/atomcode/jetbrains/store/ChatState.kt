package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.protocol.ChatEvent
import com.atomcode.jetbrains.protocol.SessionDetail
import kotlinx.collections.immutable.ImmutableList
import kotlinx.collections.immutable.persistentListOf
import kotlinx.collections.immutable.toImmutableList

// ── State ──

data class ChatState(
    val tabId: String,
    val sessionId: String? = null,
    val generation: GenerationState = GenerationState.Idle,
    val messages: ImmutableList<Message> = persistentListOf(),
    val queue: ImmutableList<QueuedPrompt> = persistentListOf(),
    val pendingPermission: PermissionRequestState? = null,
    val tokens: TokenUsageState? = null,
    val draft: String = "",
)

data class PermissionRequestState(
    val sessionId: String,
    val toolName: String,
    val reason: String,
    val callId: String,
    val arguments: String,
    val severity: PermissionSeverityState = PermissionSeverityState.Destructive,
    val allowPersist: Boolean = false,
)

data class TokenUsageState(
    val prompt: Int,
    val completion: Int,
    val total: Int,
)

data class QueuedPrompt(
    val id: String,
    val text: String,
)

sealed interface GenerationState {
    data object Idle : GenerationState
    data class Streaming(val assistantMessageId: String) : GenerationState
    data class WaitingPermission(val callId: String) : GenerationState
    data object Stopping : GenerationState
    data class Failed(val message: String) : GenerationState
}

sealed interface Message {
    val id: String

    data class User(
        override val id: String,
        val text: String,
        val images: List<ImageRef> = emptyList(),
        val contextFiles: List<String> = emptyList(),
    ) : Message

    data class Assistant(
        override val id: String,
        val markdown: String = "",
        val reasoning: String = "",
        val toolCalls: ImmutableList<ToolCallEntry> = persistentListOf(),
        val artifacts: ImmutableList<ArtifactEntry> = persistentListOf(),
        val status: AssistantStatus = AssistantStatus.Streaming,
    ) : Message
}

data class ImageRef(val mediaType: String, val data: String)

data class ToolCallEntry(
    val callId: String,
    val name: String,
    val arguments: String,
    val output: String = "",
    val success: Boolean = false,
    val durationMs: Long = 0,
    val status: ToolStatus = ToolStatus.Queued,
)

data class ArtifactEntry(
    val id: String,
    val artifactType: String,
    val title: String? = null,
    val language: String? = null,
    val content: String = "",
    val status: ArtifactStatus = ArtifactStatus.Started,
)

enum class AssistantStatus { Streaming, Complete }
enum class ToolStatus { Queued, Running, Success, Error }
enum class ArtifactStatus { Started, Streaming, Complete }

// ── Action ──

sealed interface ChatAction {
    data class SubmitPrompt(
        val text: String,
        val images: List<ImageRef> = emptyList(),
        val contextFiles: List<String> = emptyList(),
    ) : ChatAction
    data object StopGeneration : ChatAction
    data class PermissionDecision(val callId: String, val decision: PermissionDecisionKind) : ChatAction
    data class LoadSession(val detail: SessionDetail) : ChatAction
    data class DaemonEvent(val event: ChatEvent) : ChatAction
}

enum class PermissionDecisionKind { Allow, Deny, AlwaysAllow, AllowPersist }
enum class PermissionSeverityState { Safe, Destructive, Critical }
