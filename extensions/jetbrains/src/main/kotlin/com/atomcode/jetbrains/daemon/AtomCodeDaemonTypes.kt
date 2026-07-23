package com.atomcode.jetbrains.daemon

data class HealthResponse(
    val status: String,
    val version: String,
    val service: String,
    val binaryHash: String? = null,
    val instanceId: String? = null,
)

data class ProjectState(
    val workingDir: String,
    val name: String,
)

data class ChangeDirResponse(
    val success: Boolean,
    val message: String,
    val currentDir: String,
    val projectHash: String,
)

data class ConfigResponse(
    val path: String,
    val defaultProvider: String?,
    val providerCount: Int,
)

data class AuthStatusResponse(
    val loggedIn: Boolean,
    val expired: Boolean = false,
    val authPath: String,
    val userName: String?,
)

data class LoginStartResponse(
    val loginId: String,
    val url: String,
    val expiresInSeconds: Int,
    val daemonInstanceId: String? = null,
)

data class LoginPollResponse(
    val status: String,
    val userName: String?,
    val code: String? = null,
    val message: String? = null,
    val retryAfterMs: Int? = null,
)

data class ProviderInfo(
    val name: String,
    val type: String,
    val model: String,
    val isDefault: Boolean,
    val hasApiKey: Boolean,
    val requiresLogin: Boolean? = null,
    val thinkingEnabled: Boolean,
    val thinkingBudget: Int?,
    val thinkingType: String?,
    val thinkingKeep: String?,
) {
    override fun toString(): String {
        val marker = if (isDefault) " *" else ""
        val thinking = if (thinkingEnabled) " thinking" else ""
        return "$name - $model$marker$thinking"
    }
}

data class CreateProviderRequest(
    val name: String,
    val type: String,
    val model: String,
    val apiKey: String?,
    val baseUrl: String?,
    val setDefault: Boolean,
)

data class PatchProviderRequest(
    val originalName: String,
    val name: String,
    val type: String,
    val model: String,
    val apiKey: String?,
    val clearApiKey: Boolean,
    val baseUrl: String?,
    val clearBaseUrl: Boolean,
)

data class PatchThinkingRequest(
    val enabled: Boolean,
    val budget: Int?,
    val type: String?,
    val keep: String?,
)

data class ProvidersResponse(
    val defaultProvider: String,
    val providers: List<ProviderInfo>,
)

data class ModelInfo(
    val provider: String,
    val model: String,
    val providerType: String,
    val isDefault: Boolean,
) {
    override fun toString(): String {
        val marker = if (isDefault) " *" else ""
        return "$provider - $model$marker"
    }
}

data class CodingPlanSetupResponse(
    val success: Boolean,
    val reportText: String,
    val defaultProvider: String,
)

data class SetupSnapshot(
    val auth: AuthStatusResponse?,
    val providers: List<ProviderInfo>,
    val models: List<ModelInfo>,
    val defaultProvider: String,
    val currentModel: String,
    val setupRequired: Boolean,
)

data class SessionRef(
    val id: String,
    val name: String,
    val workingDir: String,
    val projectHash: String,
)

data class SessionMeta(
    val id: String,
    val name: String,
    val projectHash: String,
    val updatedAt: Long,
    val messageCount: Int,
) {
    val displayName: String
        get() = name.ifBlank { id.take(8) }

    override fun toString(): String = "$displayName ($messageCount)"
}

data class SessionDetail(
    val id: String,
    val name: String,
    val workingDir: String,
    val projectHash: String,
    val messages: List<MessageInfo>,
)

data class MessageInfo(
    val role: String,
    val content: String,
    val synthetic: Boolean = false,
    val internalOrigin: String? = null,
    val toolCalls: List<ToolCallInfo> = emptyList(),
)

data class ToolCallInfo(
    val id: String?,
    val name: String,
    val arguments: String,
)

data class ChatRequest(
    val message: String,
    val workingDir: String,
    val sessionId: String,
    val provider: String? = null,
    val images: List<ImageInput> = emptyList(),
    val approvalMode: String? = null,
)

enum class ApprovalMode(val wire: String) {
    Build("build"),
    AcceptEdits("accept_edits"),
    Auto("bypass"),
    Plan("plan");

    override fun toString(): String = when (this) {
        Build -> "Build"
        AcceptEdits -> "Accept Edits"
        Auto -> "Auto"
        Plan -> "Plan"
    }
}

data class ApprovalModeResponse(
    val ok: Boolean,
    val mode: String,
)

data class ImageInput(
    val mediaType: String,
    val data: String,
)

data class StopChatResponse(
    val success: Boolean,
    val message: String,
)

data class PermissionDecisionResponse(
    val success: Boolean,
    val error: String?,
)

interface ChatStreamListener {
    fun onEvent(event: ChatEvent) = Unit
    fun onComplete() = Unit
    fun onError(message: String) = Unit
}

sealed interface ChatEvent {
    data class RuntimeInfo(val provider: String, val model: String) : ChatEvent
    data class Text(val content: String) : ChatEvent
    data class Reasoning(val content: String) : ChatEvent
    data class ToolBatch(val callsJson: String) : ChatEvent
    data class ToolStart(val id: String?, val name: String, val arguments: String) : ChatEvent
    data class ToolOutput(val chunk: String) : ChatEvent
    data class ToolProgress(val id: String?, val progress: String) : ChatEvent
    data class ToolResult(
        val id: String?,
        val name: String,
        val output: String,
        val success: Boolean,
        val durationMs: Long,
    ) : ChatEvent
    data class ArtifactStart(val id: String, val artifactType: String, val language: String?, val title: String?) : ChatEvent
    data class ArtifactContent(val id: String, val content: String) : ChatEvent
    data class ArtifactEnd(val id: String) : ChatEvent
    data class PermissionRequest(
        val sessionId: String,
        val toolName: String,
        val reason: String,
        val callId: String,
        val arguments: String,
    ) : ChatEvent
    data class Tokens(val prompt: Int, val completion: Int, val total: Int) : ChatEvent
    data class Warning(val message: String) : ChatEvent
    data class Done(val tokens: Int, val toolCalls: Int, val sessionId: String?) : ChatEvent
    data object Stopped : ChatEvent
    data class Error(val message: String) : ChatEvent
    data class Unknown(val type: String) : ChatEvent
}

enum class ConnectionErrorKind {
    MissingBinary,
    PortUsedByNonAtomCode,
    IncompatibleDaemon,
    AuthFailed,
    LegacyUnauthenticatedDaemon,
    StartFailed,
    Timeout,
    ProviderMissing,
    Unknown,
}

sealed interface ConnectionState {
    data object Idle : ConnectionState
    data object CheckingDaemon : ConnectionState
    data class SetupRequired(val reason: String) : ConnectionState
    data object StartingDaemon : ConnectionState
    data object Connecting : ConnectionState
    data object SyncingProject : ConnectionState
    data object CheckingProvider : ConnectionState
    data class ProviderMissing(val configPath: String?) : ConnectionState
    data class Ready(val daemonVersion: String, val projectPath: String) : ConnectionState
    data class Error(val kind: ConnectionErrorKind, val message: String) : ConnectionState
}

data class DaemonAuth(val token: String?)

data class BinaryResolution(val path: String, val argsPrefix: List<String>)
