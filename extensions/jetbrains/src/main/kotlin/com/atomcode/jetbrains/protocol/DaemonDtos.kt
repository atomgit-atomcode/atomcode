package com.atomcode.jetbrains.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

// ── Health ──

@Serializable
data class HealthResponse(
    val status: String,
    val version: String,
    val service: String
)

// ── Chat ──

@Serializable
data class ChatRequest(
    val message: String,
    @SerialName("working_dir") val workingDir: String? = null,
    val provider: String? = null,
    @SerialName("session_id") val sessionId: String? = null,
    val images: List<ImageInput> = emptyList()
)

@Serializable
data class ImageInput(
    @SerialName("media_type") val mediaType: String,
    val data: String
)

@Serializable
data class StopChatRequest(@SerialName("session_id") val sessionId: String)

@Serializable
data class StopChatResponse(
    val success: Boolean,
    val message: String
)

@Serializable
data class PermissionDecisionRequest(
    @SerialName("session_id") val sessionId: String,
    val decision: String,
    @SerialName("tool_name") val toolName: String? = null
)

// ── Session ──

@Serializable
data class CreateSessionRequest(
    @SerialName("working_dir") val workingDir: String? = null,
    val title: String? = null
)

@Serializable
data class CreateSessionResponse(
    val id: String,
    val name: String,
    @SerialName("working_dir") val workingDir: String,
    @SerialName("project_hash") val projectHash: String,
    @SerialName("created_at") val createdAt: Long
)

@Serializable
data class SessionMeta(
    val id: String,
    val name: String,
    @SerialName("working_dir") val workingDir: String,
    @SerialName("created_at") val createdAt: Long,
    @SerialName("updated_at") val updatedAt: Long,
    @SerialName("message_count") val messageCount: Int
)

@Serializable
data class SessionDetail(
    val id: String,
    val name: String,
    @SerialName("working_dir") val workingDir: String,
    @SerialName("created_at") val createdAt: Long,
    @SerialName("updated_at") val updatedAt: Long,
    @SerialName("message_count") val messageCount: Int,
    val messages: List<MessageInfo>
)

@Serializable
data class MessageInfo(
    val role: String,
    val content: String,
    @SerialName("tool_calls") val toolCalls: List<ToolCallInfo>? = null,
    @SerialName("tool_result") val toolResult: ToolResultInfo? = null,
    val artifacts: List<ArtifactInfo>? = null,
    val images: List<ImageData>? = null
)

@Serializable
data class ToolCallInfo(
    val id: String,
    val name: String,
    val arguments: String,
    val display: String
)

@Serializable
data class ToolResultInfo(
    @SerialName("call_id") val callId: String,
    val success: Boolean,
    val summary: String,
    @SerialName("line_count") val lineCount: Int
)

@Serializable
data class ArtifactInfo(
    val id: String,
    @SerialName("artifact_type") val artifactType: String,
    val title: String? = null,
    val language: String? = null,
    val content: String
)

@Serializable
data class ImageData(
    @SerialName("media_type") val mediaType: String,
    val data: String
)

@Serializable
data class RenameRequest(val name: String)

@Serializable
data class AppendSessionMessagesRequest(
    @SerialName("working_dir") val workingDir: String? = null,
    val messages: List<AppendSessionMessage>
)

@Serializable
data class AppendSessionMessage(val role: String, val content: String)

@Serializable
data class AppendSessionMessagesResponse(
    val success: Boolean,
    @SerialName("session_id") val sessionId: String,
    @SerialName("message_count") val messageCount: Int,
    @SerialName("project_hash") val projectHash: String
)

// ── Project ──

@Serializable
data class ProjectState(
    @SerialName("working_dir") val workingDir: String,
    @SerialName("previous_dir") val previousDir: String? = null,
    @SerialName("recent_dirs") val recentDirs: List<String> = emptyList(),
    val name: String
)

@Serializable
data class ProjectInfo(
    val hash: String,
    val name: String,
    @SerialName("working_dir") val workingDir: String,
    val description: String? = null,
    @SerialName("session_count") val sessionCount: Int,
    @SerialName("created_at") val createdAt: Long,
    @SerialName("last_updated") val lastUpdated: Long
)

@Serializable
data class ChangeDirRequest(
    val path: String,
    @SerialName("set_default") val setDefault: Boolean = false
)

@Serializable
data class ChangeDirResponse(
    val success: Boolean,
    val message: String,
    @SerialName("current_dir") val currentDir: String,
    @SerialName("project_hash") val projectHash: String
)

// ── Provider ──

@Serializable
data class ProviderInfo(
    val name: String,
    val type: String,
    val model: String,
    @SerialName("base_url") val baseUrl: String? = null,
    @SerialName("has_api_key") val hasApiKey: Boolean = false,
    @SerialName("is_default") val isDefault: Boolean = false,
    @SerialName("context_window") val contextWindow: Int = 128000,
    @SerialName("max_tokens") val maxTokens: Int? = null,
    @SerialName("thinking_enabled") val thinkingEnabled: Boolean? = null,
    @SerialName("thinking_budget") val thinkingBudget: Int? = null,
    @SerialName("thinking_type") val thinkingType: String? = null,
    @SerialName("thinking_keep") val thinkingKeep: String? = null,
    @SerialName("reasoning_history") val reasoningHistory: String? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null,
    @SerialName("skip_tls_verify") val skipTlsVerify: Boolean = false,
    val ephemeral: Boolean = false
)

@Serializable
data class CreateProviderRequest(
    val name: String,
    val type: String,
    val model: String,
    @SerialName("api_key") val apiKey: String? = null,
    @SerialName("base_url") val baseUrl: String? = null,
    @SerialName("user_agent") val userAgent: String? = null,
    @SerialName("context_window") val contextWindow: Int? = null,
    @SerialName("max_tokens") val maxTokens: Int? = null,
    @SerialName("thinking_type") val thinkingType: String? = null,
    @SerialName("thinking_keep") val thinkingKeep: String? = null,
    @SerialName("reasoning_history") val reasoningHistory: String? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null,
    @SerialName("thinking_enabled") val thinkingEnabled: Boolean? = null,
    @SerialName("thinking_budget") val thinkingBudget: Int? = null,
    @SerialName("skip_tls_verify") val skipTlsVerify: Boolean = false,
    @SerialName("set_default") val setDefault: Boolean = false
)

@Serializable
data class PatchProviderRequest(
    val name: String? = null,
    val type: String? = null,
    val model: String? = null,
    @SerialName("api_key") val apiKey: String? = null,
    @SerialName("base_url") val baseUrl: String? = null,
    @SerialName("context_window") val contextWindow: Int? = null,
    @SerialName("max_tokens") val maxTokens: Int? = null,
    @SerialName("thinking_enabled") val thinkingEnabled: Boolean? = null,
    @SerialName("thinking_budget") val thinkingBudget: Int? = null,
    @SerialName("skip_tls_verify") val skipTlsVerify: Boolean? = null
)

@Serializable
data class PatchThinkingRequest(
    val enabled: Boolean? = null,
    val budget: Int? = null,
    val type: String? = null,
    val keep: String? = null,
    @SerialName("reasoning_history") val reasoningHistory: String? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null
)

// ── Auth ──

@Serializable
data class AuthStatusResponse(
    @SerialName("logged_in") val loggedIn: Boolean,
    @SerialName("auth_path") val authPath: String,
    val user: UserInfo? = null,
    val token: TokenInfo? = null
)

@Serializable
data class UserInfo(
    val id: String? = null,
    val name: String? = null,
    val email: String? = null
)

@Serializable
data class TokenInfo(
    @SerialName("token_type") val tokenType: String,
    @SerialName("expires_in") val expiresIn: Long? = null,
    @SerialName("created_at") val createdAt: Long,
    @SerialName("has_refresh_token") val hasRefreshToken: Boolean
)

@Serializable
data class LoginStartRequest(@SerialName("open_browser") val openBrowser: Boolean = true)

@Serializable
data class LoginStartResponse(
    @SerialName("login_id") val loginId: String,
    val url: String,
    @SerialName("expires_in_seconds") val expiresInSeconds: Long
)

@Serializable
data class LoginPollResponse(
    val status: String,
    val user: UserInfo? = null
)

// ── Model ──

@Serializable
data class ModelInfo(
    val provider: String,
    val model: String,
    @SerialName("provider_type") val providerType: String,
    @SerialName("is_default") val isDefault: Boolean = false,
    @SerialName("effort_applicable") val effortApplicable: Boolean = false,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null
)

// ── CodingPlan ──

@Serializable
data class CodingPlanSetupRequest(@SerialName("login_id") val loginId: String? = null)

@Serializable
data class CodingPlanSetupResponse(
    val success: Boolean,
    @SerialName("report_text") val reportText: String,
    @SerialName("default_provider") val defaultProvider: String,
    val providers: List<ProviderInfo> = emptyList(),
    val steps: CodingPlanSteps? = null
)

@Serializable
data class CodingPlanSteps(
    val login: StepInfo? = null,
    val claim: StepInfo? = null,
    val models: StepInfo? = null,
    val status: StepInfo? = null
)

@Serializable
data class StepInfo(val status: String, val message: String)

// ── Config ──

@Serializable
data class ConfigResponse(
    val path: String? = null,
    @SerialName("default_provider") val defaultProvider: String,
    @SerialName("default_workdir") val defaultWorkdir: String? = null,
    val providers: List<ProviderInfo> = emptyList()
)

// ── MCP ──

@Serializable
data class McpStatusResponse(val servers: List<McpServerStatus> = emptyList())

@Serializable
data class McpServerStatus(
    val name: String,
    val status: String,
    @SerialName("tool_count") val toolCount: Int? = null,
    val error: String? = null
)

// ── Skills ──

@Serializable
data class SkillInfo(val name: String, val description: String)

// ── Tunnel ──

@Serializable
data class TunnelStatus(
    @SerialName("bind_host") val bindHost: String,
    val port: Int,
    val reachable: Boolean,
    @SerialName("remote_url") val remoteUrl: String? = null,
    @SerialName("qr_svg") val qrSvg: String? = null
)

// ── FS ──

@Serializable
data class FsListResponse(
    val path: String,
    val dirs: List<String> = emptyList(),
    val files: List<String> = emptyList()
)

@Serializable
data class FsMkdirRequest(val path: String)
