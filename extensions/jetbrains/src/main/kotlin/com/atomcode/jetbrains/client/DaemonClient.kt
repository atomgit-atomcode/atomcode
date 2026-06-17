package com.atomcode.jetbrains.client

import com.atomcode.jetbrains.protocol.*
import com.google.gson.FieldNamingPolicy
import com.google.gson.GsonBuilder
import com.google.gson.reflect.TypeToken
import java.net.URI
import java.net.URLEncoder
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration

class DaemonClient(
    private val baseUrl: String,
    private val token: String? = null,
    private val httpClient: HttpClient = HttpClient.newBuilder()
        .connectTimeout(Duration.ofSeconds(30))
        .build()
) {
    private val gson = GsonBuilder()
        .setFieldNamingPolicy(FieldNamingPolicy.LOWER_CASE_WITH_UNDERSCORES)
        .create()

    // ── Internal ──

    private suspend inline fun <reified T> get(path: String): T {
        val request = requestBuilder(path).GET().build()
        val response = httpClient.send(request, HttpResponse.BodyHandlers.ofString())
        checkStatus(response)
        return gson.fromJson(response.body(), object : TypeToken<T>() {}.type)
    }

    private suspend inline fun <reified T, reified B> post(path: String, body: B): T {
        val bodyJson = gson.toJson(body)
        val request = requestBuilder(path)
            .POST(HttpRequest.BodyPublishers.ofString(bodyJson))
            .build()
        val response = httpClient.send(request, HttpResponse.BodyHandlers.ofString())
        checkStatus(response)
        return gson.fromJson(response.body(), object : TypeToken<T>() {}.type)
    }

    private suspend inline fun <reified T> patch(path: String, body: Any): T {
        val bodyJson = gson.toJson(body)
        val request = requestBuilder(path)
            .method("PATCH", HttpRequest.BodyPublishers.ofString(bodyJson))
            .build()
        val response = httpClient.send(request, HttpResponse.BodyHandlers.ofString())
        checkStatus(response)
        return gson.fromJson(response.body(), object : TypeToken<T>() {}.type)
    }

    private suspend fun delete(path: String) {
        val request = requestBuilder(path).DELETE().build()
        val response = httpClient.send(request, HttpResponse.BodyHandlers.ofString())
        checkStatus(response)
    }

    private fun requestBuilder(path: String): HttpRequest.Builder {
        val builder = HttpRequest.newBuilder()
            .uri(URI.create("$baseUrl$path"))
            .header("Content-Type", "application/json")
            .header("X-AtomCode-Client", "jetbrains")
            .timeout(Duration.ofSeconds(30))
        if (token != null) {
            builder.header("Authorization", "Bearer $token")
        }
        return builder
    }

    private fun checkStatus(response: HttpResponse<String>) {
        if (response.statusCode() >= 400) {
            throw DaemonApiException(response.statusCode(), response.uri().path, response.body().take(500))
        }
    }

    // ── Health ──

    suspend fun health(): HealthResponse = get("/health")

    suspend fun shutdown() {
        try { post<Map<String, Any>, String>("/shutdown", "") } catch (_: Exception) {}
    }

    // ── Chat ──

    fun streamChat(request: ChatRequest): ChatStream {
        val bodyJson = gson.toJson(request)
        val httpRequest = requestBuilder("/chat")
            .header("Accept", "text/event-stream")
            .POST(HttpRequest.BodyPublishers.ofString(bodyJson))
            .build()
        return ChatStream(httpClient, httpRequest)
    }

    suspend fun stopChat(sessionId: String): StopChatResponse =
        post("/chat/stop", StopChatRequest(sessionId))

    suspend fun submitPermissionDecision(request: PermissionDecisionRequest) {
        post<Map<String, Any>, PermissionDecisionRequest>("/chat/permission", request)
    }

    // ── Session ──

    suspend fun createSession(workingDir: String?, title: String?): CreateSessionResponse =
        post("/sessions", CreateSessionRequest(workingDir, title))

    suspend fun listSessions(): List<SessionMeta> = get("/sessions")

    suspend fun searchSessions(query: String): List<SessionMeta> =
        get("/sessions/search?q=${query.urlEncode()}")

    suspend fun getSessionDetail(projectHash: String, sessionId: String): SessionDetail =
        get("/projects/$projectHash/sessions/$sessionId")

    suspend fun deleteSession(projectHash: String, sessionId: String) =
        delete("/projects/$projectHash/sessions/$sessionId")

    suspend fun renameSession(projectHash: String, sessionId: String, name: String) =
        patch<SessionMeta>("/projects/$projectHash/sessions/$sessionId/rename", RenameRequest(name))

    // ── Project ──

    suspend fun getProjectState(): ProjectState = get("/project")

    suspend fun changeDir(path: String, setDefault: Boolean = false): ChangeDirResponse =
        post("/cd", ChangeDirRequest(path, setDefault))

    // ── Provider ──

    suspend fun listProviders(): List<ProviderInfo> = get("/providers")

    suspend fun createProvider(request: CreateProviderRequest) {
        post<Map<String, Any>, CreateProviderRequest>("/providers", request)
    }

    suspend fun patchProvider(name: String, request: PatchProviderRequest) =
        patch<ProviderInfo>("/providers/${name.urlEncode()}", request)

    suspend fun deleteProvider(name: String) =
        delete("/providers/${name.urlEncode()}")

    suspend fun setDefaultProvider(name: String) {
        post<Map<String, Any>, String>("/providers/${name.urlEncode()}/default", "")
    }

    suspend fun patchThinking(name: String, request: PatchThinkingRequest) =
        patch<ProviderInfo>("/providers/${name.urlEncode()}/thinking", request)

    // ── Auth ──

    suspend fun authStatus(): AuthStatusResponse = get("/auth/status")

    suspend fun startLogin(openBrowser: Boolean = true): LoginStartResponse =
        post("/auth/login/start", LoginStartRequest(openBrowser))

    suspend fun pollLogin(loginId: String): LoginPollResponse =
        post("/auth/login/$loginId/poll", "")

    suspend fun cancelLogin(loginId: String) = delete("/auth/login/$loginId")

    suspend fun logout() {
        post<Map<String, Any>, String>("/auth/logout", "")
    }

    // ── CodingPlan ──

    suspend fun codingPlanSetup(loginId: String? = null): CodingPlanSetupResponse =
        post("/codingplan/setup", CodingPlanSetupRequest(loginId))

    // ── Models ──

    suspend fun listModels(): List<ModelInfo> = get("/models")

    // ── MCP ──

    suspend fun mcpStatus(): McpStatusResponse = get("/mcp/status")

    suspend fun mcpReload() {
        post<Map<String, Any>, String>("/mcp/reload", "")
    }

    // ── Skills ──

    suspend fun listSkills(): List<SkillInfo> = get("/skills")

    // ── Config ──

    suspend fun getConfig(): ConfigResponse = get("/config")

    suspend fun reloadConfig(): ConfigResponse = post("/config/reload", "")

    private fun String.urlEncode(): String =
        URLEncoder.encode(this, Charsets.UTF_8).replace("+", "%20")
}

class DaemonApiException(code: Int, path: String, body: String) :
    RuntimeException("Daemon returned HTTP $code for $path: $body")
