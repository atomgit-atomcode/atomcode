package com.atomcode.jetbrains.store

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class ChatViewModel(
    val messages: List<MessageViewModel>,
    @SerialName("is_generating") val isGenerating: Boolean,
    @SerialName("is_waiting_permission") val isWaitingPermission: Boolean,
    val tokens: TokenUsageViewModel? = null,
    @SerialName("pending_permission") val pendingPermission: PermissionViewModel? = null,
    @SerialName("session_id") val sessionId: String? = null,
    @SerialName("generation_error") val generationError: String? = null,
)

@Serializable
data class MessageViewModel(
    val id: String,
    val role: String,
    val text: String = "",
    val reasoning: String? = null,
    val status: String? = null,
    @SerialName("tool_calls") val toolCalls: List<ToolCallViewModel> = emptyList(),
    val artifacts: List<ArtifactViewModel> = emptyList(),
    val images: List<ImageViewModel> = emptyList(),
    @SerialName("context_files") val contextFiles: List<String> = emptyList(),
    val queued: Boolean = false,
)

@Serializable
data class ToolCallViewModel(
    @SerialName("call_id") val callId: String,
    val name: String,
    val arguments: String,
    val output: String = "",
    val success: Boolean = false,
    @SerialName("duration_ms") val durationMs: Long = 0,
    val status: String = "queued",
)

@Serializable
data class ArtifactViewModel(
    val id: String,
    @SerialName("artifact_type") val artifactType: String,
    val title: String? = null,
    val language: String? = null,
    val content: String = "",
    val status: String = "started",
)

@Serializable
data class TokenUsageViewModel(
    val prompt: Int, val completion: Int, val total: Int,
)

@Serializable
data class PermissionViewModel(
    @SerialName("tool_name") val toolName: String,
    val reason: String,
    @SerialName("call_id") val callId: String,
    val arguments: String,
    val severity: String = "destructive",
    @SerialName("allow_persist") val allowPersist: Boolean = false,
)

@Serializable
data class ImageViewModel(
    @SerialName("media_type") val mediaType: String,
    val data: String,
)

// ── Mapping ──

fun ChatState.toViewModel(): ChatViewModel = ChatViewModel(
    messages = messages.map { it.toViewModel() },
    isGenerating = generation is GenerationState.Streaming,
    isWaitingPermission = generation is GenerationState.WaitingPermission,
    tokens = tokens?.let { TokenUsageViewModel(it.prompt, it.completion, it.total) },
    pendingPermission = pendingPermission?.let {
        PermissionViewModel(
            toolName = it.toolName, reason = it.reason, callId = it.callId,
            arguments = it.arguments,
            severity = when (it.severity) {
                PermissionSeverityState.Safe -> "safe"
                PermissionSeverityState.Destructive -> "destructive"
                PermissionSeverityState.Critical -> "critical"
            },
            allowPersist = it.allowPersist,
        )
    },
    sessionId = sessionId,
    generationError = (generation as? GenerationState.Failed)?.message,
)

fun Message.toViewModel(): MessageViewModel = when (this) {
    is Message.User -> MessageViewModel(
        id = id, role = "user", text = text,
        images = images.map { ImageViewModel(it.mediaType, it.data) },
        contextFiles = contextFiles,
    )
    is Message.Assistant -> MessageViewModel(
        id = id, role = "assistant", text = markdown,
        reasoning = reasoning.takeIf { it.isNotEmpty() },
        status = when (status) { AssistantStatus.Streaming -> "streaming"; AssistantStatus.Complete -> "complete" },
        toolCalls = toolCalls.map { it.toViewModel() },
        artifacts = artifacts.map { it.toViewModel() },
    )
}

fun ToolCallEntry.toViewModel(): ToolCallViewModel = ToolCallViewModel(
    callId = callId, name = name, arguments = arguments,
    output = output, success = success, durationMs = durationMs,
    status = when (status) {
        ToolStatus.Queued -> "queued"; ToolStatus.Running -> "running"
        ToolStatus.Success -> "success"; ToolStatus.Error -> "error"
    },
)

fun ArtifactEntry.toViewModel(): ArtifactViewModel = ArtifactViewModel(
    id = id, artifactType = artifactType, title = title,
    language = language, content = content,
    status = when (status) {
        ArtifactStatus.Started -> "started"; ArtifactStatus.Streaming -> "streaming"
        ArtifactStatus.Complete -> "complete"
    },
)
