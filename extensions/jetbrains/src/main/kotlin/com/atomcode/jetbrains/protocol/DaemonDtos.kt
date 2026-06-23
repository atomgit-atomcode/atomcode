package com.atomcode.jetbrains.protocol

/**
 * Daemon REST API 请求/响应 DTO。
 * 使用 Gson 序列化，字段名自动转换为 snake_case（与 daemon Rust serde 输出一致）。
 * DaemonClient 使用 GsonBuilder().setFieldNamingPolicy(LOWER_CASE_WITH_UNDERSCORES) 配置。
 */

// ── Health ──

data class HealthResponse(
    val status: String,
    val version: String,
    val service: String
)

// ── Chat ──

data class ChatRequest(
    val message: String,
    val workingDir: String? = null,
    val provider: String? = null,
    val sessionId: String? = null,
    val images: List<ImageInput> = emptyList()
)

data class ImageInput(
    val mediaType: String,
    val data: String
)

data class StopChatRequest(val sessionId: String)

data class StopChatResponse(
    val success: Boolean,
    val message: String
)

data class PermissionDecisionRequest(
    val sessionId: String,
    val decision: String,
    val toolName: String? = null
)

// ── Session ──

data class CreateSessionRequest(
    val workingDir: String? = null,
    val title: String? = null
)

data class CreateSessionResponse(
    val id: String,
    val name: String,
    val workingDir: String,
    val projectHash: String,
    val createdAt: Long
)

data class SessionMeta(
    val id: String,
    val name: String,
    val workingDir: String,
    val createdAt: Long,
    val updatedAt: Long,
    val messageCount: Int
)

data class SessionDetail(
    val id: String,
    val name: String,
    val workingDir: String,
    val createdAt: Long,
    val updatedAt: Long,
    val messageCount: Int,
    val messages: List<MessageInfo>
)

data class MessageInfo(
    val role: String,
    val content: String,
    val toolCalls: List<ToolCallInfo>? = null,
    val toolResult: ToolResultInfo? = null,
    val artifacts: List<ArtifactInfo>? = null,
    val images: List<ImageData>? = null
)

data class ToolCallInfo(
    val id: String,
    val name: String,
    val arguments: String,
    val display: String
)

data class ToolResultInfo(
    val callId: String,
    val success: Boolean,
    val summary: String,
    val lineCount: Int
)

data class ArtifactInfo(
    val id: String,
    val artifactType: String,
    val title: String? = null,
    val language: String? = null,
    val content: String
)

data class ImageData(
    val mediaType: String,
    val data: String
)

data class RenameRequest(val name: String)

data class AppendSessionMessagesRequest(
    val workingDir: String? = null,
    val messages: List<AppendSessionMessage>
)

data class AppendSessionMessage(val role: String, val content: String)

data class AppendSessionMessagesResponse(
    val success: Boolean,
    val sessionId: String,
    val messageCount: Int,
    val projectHash: String
)

// ── Project ──

data class ProjectState(
    val workingDir: String,
    val previousDir: String? = null,
    val recentDirs: List<String> = emptyList(),
    val name: String
)

data class ProjectInfo(
    val hash: String,
    val name: String,
    val workingDir: String,
    val description: String? = null,
    val sessionCount: Int,
    val createdAt: Long,
    val lastUpdated: Long
)

data class ChangeDirRequest(
    val path: String,
    val setDefault: Boolean = false
)

data class ChangeDirResponse(
    val success: Boolean,
    val message: String,
    val currentDir: String,
    val projectHash: String
)

// ── Provider ──

data class ProviderInfo(
    val name: String,
    val type: String,
    val model: String,
    val baseUrl: String? = null,
    val hasApiKey: Boolean = false,
    val isDefault: Boolean = false,
    val contextWindow: Int = 128000,
    val maxTokens: Int? = null,
    val thinkingEnabled: Boolean? = null,
    val thinkingBudget: Int? = null,
    val thinkingType: String? = null,
    val thinkingKeep: String? = null,
    val reasoningHistory: String? = null,
    val reasoningEffort: String? = null,
    val skipTlsVerify: Boolean = false,
    val ephemeral: Boolean = false
)

data class CreateProviderRequest(
    val name: String,
    val type: String,
    val model: String,
    val apiKey: String? = null,
    val baseUrl: String? = null,
    val userAgent: String? = null,
    val contextWindow: Int? = null,
    val maxTokens: Int? = null,
    val thinkingType: String? = null,
    val thinkingKeep: String? = null,
    val reasoningHistory: String? = null,
    val reasoningEffort: String? = null,
    val thinkingEnabled: Boolean? = null,
    val thinkingBudget: Int? = null,
    val skipTlsVerify: Boolean = false,
    val setDefault: Boolean = false
)

data class PatchProviderRequest(
    val name: String? = null,
    val type: String? = null,
    val model: String? = null,
    val apiKey: String? = null,
    val baseUrl: String? = null,
    val contextWindow: Int? = null,
    val maxTokens: Int? = null,
    val thinkingEnabled: Boolean? = null,
    val thinkingBudget: Int? = null,
    val skipTlsVerify: Boolean? = null
)

data class PatchThinkingRequest(
    val enabled: Boolean? = null,
    val budget: Int? = null,
    val type: String? = null,
    val keep: String? = null,
    val reasoningHistory: String? = null,
    val reasoningEffort: String? = null
)

// ── Auth ──

data class AuthStatusResponse(
    val loggedIn: Boolean,
    val authPath: String,
    val user: UserInfo? = null,
    val token: TokenInfo? = null
)

data class UserInfo(
    val id: String? = null,
    val name: String? = null,
    val email: String? = null
)

data class TokenInfo(
    val tokenType: String,
    val expiresIn: Long? = null,
    val createdAt: Long,
    val hasRefreshToken: Boolean
)

data class LoginStartRequest(val openBrowser: Boolean = true)

data class LoginStartResponse(
    val loginId: String,
    val url: String,
    val expiresInSeconds: Long
)

data class LoginPollResponse(
    val status: String,
    val user: UserInfo? = null
)

// ── Model ──

data class ModelInfo(
    val provider: String,
    val model: String,
    val providerType: String,
    val isDefault: Boolean = false,
    val effortApplicable: Boolean = false,
    val reasoningEffort: String? = null
)

// ── CodingPlan ──

data class CodingPlanSetupRequest(val loginId: String? = null)

data class CodingPlanSetupResponse(
    val success: Boolean,
    val reportText: String,
    val defaultProvider: String,
    val providers: List<ProviderInfo> = emptyList(),
    val steps: CodingPlanSteps? = null
)

data class CodingPlanSteps(
    val login: StepInfo? = null,
    val claim: StepInfo? = null,
    val models: StepInfo? = null,
    val status: StepInfo? = null
)

data class StepInfo(val status: String, val message: String)

// ── Config ──

data class ConfigResponse(
    val path: String? = null,
    val defaultProvider: String,
    val defaultWorkdir: String? = null,
    val providers: List<ProviderInfo> = emptyList()
)

// ── MCP ──

data class McpStatusResponse(val servers: List<McpServerStatus> = emptyList())

data class McpServerStatus(
    val name: String,
    val status: String,
    val toolCount: Int? = null,
    val error: String? = null
)

// ── Skills ──

data class SkillInfo(val name: String, val description: String)

// ── Tunnel ──

data class TunnelStatus(
    val bindHost: String,
    val port: Int,
    val reachable: Boolean,
    val remoteUrl: String? = null,
    val qrSvg: String? = null
)

// ── FS ──

data class FsListResponse(
    val path: String,
    val dirs: List<String> = emptyList(),
    val files: List<String> = emptyList()
)

data class FsMkdirRequest(val path: String)
