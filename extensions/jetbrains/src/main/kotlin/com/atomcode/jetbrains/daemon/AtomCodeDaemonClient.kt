package com.atomcode.jetbrains.daemon

import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration
import java.util.concurrent.CompletableFuture

class AtomCodeDaemonClient(
    private val host: String,
    private val port: Int,
    private val timeoutMs: Int,
    private val auth: DaemonAuth = DaemonAuth(null),
) {
    private val client = HttpClient.newBuilder()
        .connectTimeout(Duration.ofMillis(timeoutMs.toLong()))
        .build()

    private val baseUrl = "http://$host:$port"

    fun health(): CompletableFuture<HealthResponse> =
        send("GET", "/health").thenApply {
            HealthResponse(
                status = it.jsonString("status").orEmpty(),
                version = it.jsonString("version").orEmpty(),
                service = it.jsonString("service").orEmpty(),
                binaryHash = it.jsonString("binary_hash"),
            )
        }

    fun shutdown(): CompletableFuture<Boolean> =
        send("POST", "/shutdown", "{}").thenApply {
            it.jsonBoolean("success") ?: true
        }

    fun project(): CompletableFuture<ProjectState> =
        send("GET", "/project").thenApply {
            ProjectState(
                workingDir = it.jsonString("working_dir").orEmpty(),
                name = it.jsonString("name").orEmpty(),
            )
        }

    fun changeDir(path: String): CompletableFuture<ChangeDirResponse> =
        send("POST", "/cd", """{"path":${path.jsonQuoted()}}""").thenApply {
            ChangeDirResponse(
                success = it.jsonBoolean("success") ?: false,
                message = it.jsonString("message").orEmpty(),
                currentDir = it.jsonString("current_dir").orEmpty(),
                projectHash = it.jsonString("project_hash").orEmpty(),
            )
        }

    fun config(): CompletableFuture<ConfigResponse> =
        send("GET", "/config").thenApply {
            ConfigResponse(
                path = it.jsonString("path").orEmpty(),
                defaultProvider = it.jsonString("default_provider"),
                providerCount = it.jsonArrayObjects("providers").size,
            )
        }

    fun authStatus(): CompletableFuture<AuthStatusResponse> =
        send("GET", "/auth/status").thenApply { raw ->
            AuthStatusResponse(
                loggedIn = raw.jsonBoolean("logged_in") ?: false,
                authPath = raw.jsonString("auth_path").orEmpty(),
                userName = raw.jsonNestedObject("user")?.let {
                    it.jsonString("name") ?: it.jsonString("username") ?: it.jsonString("email")
                },
            )
        }

    fun startLogin(openBrowser: Boolean = true): CompletableFuture<LoginStartResponse> =
        send("POST", "/auth/login/start", """{"open_browser":$openBrowser}""").thenApply { raw ->
            LoginStartResponse(
                loginId = raw.jsonString("login_id").orEmpty(),
                url = raw.jsonString("url").orEmpty(),
                expiresInSeconds = raw.jsonInt("expires_in_seconds") ?: 600,
            )
        }

    fun pollLogin(loginId: String): CompletableFuture<LoginPollResponse> =
        send("POST", "/auth/login/${loginId.urlPathEncoded()}/poll").thenApply { raw ->
            LoginPollResponse(
                status = raw.jsonString("status").orEmpty(),
                userName = raw.jsonNestedObject("user")?.let {
                    it.jsonString("name") ?: it.jsonString("username") ?: it.jsonString("email")
                },
            )
        }

    fun cancelLogin(loginId: String): CompletableFuture<Boolean> =
        send("DELETE", "/auth/login/${loginId.urlPathEncoded()}").thenApply {
            it.jsonBoolean("success") ?: false
        }

    fun listProviders(): CompletableFuture<ProvidersResponse> =
        send("GET", "/providers").thenApply { raw ->
            ProvidersResponse(
                defaultProvider = raw.jsonString("default_provider").orEmpty(),
                providers = raw.jsonArrayObjects("providers").map { it.toProviderInfo() },
            )
        }

    fun createProvider(request: CreateProviderRequest): CompletableFuture<ProviderInfo> {
        val body = buildString {
            append("{")
            append("\"name\":${request.name.jsonQuoted()},")
            append("\"type\":${request.type.jsonQuoted()},")
            append("\"model\":${request.model.jsonQuoted()},")
            append("\"set_default\":${request.setDefault}")
            request.apiKey?.takeIf { it.isNotBlank() }?.let {
                append(",\"api_key\":${it.jsonQuoted()}")
            }
            request.baseUrl?.takeIf { it.isNotBlank() }?.let {
                append(",\"base_url\":${it.jsonQuoted()}")
            }
            append("}")
        }
        return send("POST", "/providers", body).thenApply { it.toProviderInfo() }
    }

    fun patchProvider(request: PatchProviderRequest): CompletableFuture<ProviderInfo> {
        val body = buildString {
            append("{")
            append("\"name\":${request.name.jsonQuoted()},")
            append("\"type\":${request.type.jsonQuoted()},")
            append("\"model\":${request.model.jsonQuoted()},")
            append("\"clear_api_key\":${request.clearApiKey},")
            append("\"clear_base_url\":${request.clearBaseUrl}")
            request.apiKey?.takeIf { it.isNotBlank() }?.let {
                append(",\"api_key\":${it.jsonQuoted()}")
            }
            request.baseUrl?.takeIf { it.isNotBlank() }?.let {
                append(",\"base_url\":${it.jsonQuoted()}")
            }
            append("}")
        }
        return send("PATCH", "/providers/${request.originalName.urlPathEncoded()}", body).thenApply { it.toProviderInfo() }
    }

    fun patchThinking(name: String, request: PatchThinkingRequest): CompletableFuture<ProviderInfo> {
        val body = buildString {
            append("{")
            append("\"enabled\":${request.enabled}")
            request.budget?.let {
                append(",\"budget\":$it")
            }
            request.type?.let {
                append(",\"type\":${it.jsonQuoted()}")
            }
            request.keep?.let {
                append(",\"keep\":${it.jsonQuoted()}")
            }
            append("}")
        }
        return send("PATCH", "/providers/${name.urlPathEncoded()}/thinking", body).thenApply { it.toProviderInfo() }
    }

    fun deleteProvider(name: String): CompletableFuture<ProvidersResponse> =
        send("DELETE", "/providers/${name.urlPathEncoded()}").thenApply { raw ->
            ProvidersResponse(
                defaultProvider = raw.jsonString("default_provider").orEmpty(),
                providers = raw.jsonArrayObjects("providers").map { it.toProviderInfo() },
            )
        }

    fun listModels(): CompletableFuture<List<ModelInfo>> =
        send("GET", "/models").thenApply { raw ->
            raw.jsonObjects().map {
                ModelInfo(
                    provider = it.jsonString("provider").orEmpty(),
                    model = it.jsonString("model").orEmpty(),
                    providerType = it.jsonString("provider_type").orEmpty(),
                    isDefault = it.jsonBoolean("is_default") ?: false,
                )
            }.filter { it.provider.isNotBlank() }
        }

    fun setDefaultProvider(name: String): CompletableFuture<ConfigResponse> =
        send("POST", "/providers/${name.urlPathEncoded()}/default").thenApply {
            ConfigResponse(
                path = it.jsonString("path").orEmpty(),
                defaultProvider = it.jsonString("default_provider"),
                providerCount = it.jsonArrayObjects("providers").size,
            )
        }

    fun setupCodingPlan(): CompletableFuture<CodingPlanSetupResponse> =
        send("POST", "/codingplan/setup", "{}").thenApply {
            CodingPlanSetupResponse(
                success = it.jsonBoolean("success") ?: false,
                reportText = it.jsonString("report_text").orEmpty(),
                defaultProvider = it.jsonString("default_provider").orEmpty(),
            )
        }

    fun createSession(title: String?, workingDir: String): CompletableFuture<SessionRef> =
        send("POST", "/sessions", """{"title":${title.jsonQuotedOrNull()},"working_dir":${workingDir.jsonQuoted()}}""").thenApply {
            SessionRef(
                id = it.jsonString("id").orEmpty(),
                name = it.jsonString("name").orEmpty(),
                workingDir = it.jsonString("working_dir").orEmpty(),
                projectHash = it.jsonString("project_hash").orEmpty(),
            )
        }

    fun listSessions(): CompletableFuture<List<SessionMeta>> =
        send("GET", "/sessions").thenApply(::parseSessionMetaList)

    fun searchSessions(query: String): CompletableFuture<List<SessionMeta>> {
        val trimmed = query.trim()
        if (trimmed.isEmpty()) return listSessions()
        return send("GET", "/sessions/search?q=${trimmed.urlQueryEncoded()}").thenApply(::parseSessionMetaList)
    }

    fun getSession(projectHash: String, sessionId: String): CompletableFuture<SessionDetail> =
        send("GET", "/projects/${projectHash.urlPathEncoded()}/sessions/${sessionId.urlPathEncoded()}").thenApply { raw ->
            SessionDetail(
                id = raw.jsonString("id").orEmpty(),
                name = raw.jsonString("name").orEmpty(),
                workingDir = raw.jsonString("working_dir").orEmpty(),
                projectHash = projectHash,
                messages = raw.jsonArrayObjects("messages").map {
                    MessageInfo(
                        role = it.jsonString("role").orEmpty(),
                        content = it.jsonString("content").orEmpty(),
                    )
                },
            )
        }

    fun renameSession(projectHash: String, sessionId: String, name: String): CompletableFuture<String> =
        send(
            "PATCH",
            "/projects/${projectHash.urlPathEncoded()}/sessions/${sessionId.urlPathEncoded()}/rename",
            """{"name":${name.jsonQuoted()}}""",
        )

    fun deleteSession(projectHash: String, sessionId: String): CompletableFuture<String> =
        send("DELETE", "/projects/${projectHash.urlPathEncoded()}/sessions/${sessionId.urlPathEncoded()}")

    fun stopChat(sessionId: String): CompletableFuture<StopChatResponse> =
        send("POST", "/chat/stop", """{"session_id":${sessionId.jsonQuoted()}}""").thenApply {
            StopChatResponse(
                success = it.jsonBoolean("success") ?: false,
                message = it.jsonString("message").orEmpty(),
            )
        }

    fun sendPermissionDecision(
        sessionId: String,
        decision: String,
        toolName: String? = null,
    ): CompletableFuture<PermissionDecisionResponse> {
        val body = buildString {
            append("{")
            append("\"session_id\":${sessionId.jsonQuoted()},")
            append("\"decision\":${decision.jsonQuoted()}")
            toolName?.takeIf { it.isNotBlank() }?.let {
                append(",\"tool_name\":${it.jsonQuoted()}")
            }
            append("}")
        }
        return send("POST", "/chat/permission", body).thenApply {
            PermissionDecisionResponse(
                success = it.jsonBoolean("success") ?: false,
                error = it.jsonString("error"),
            )
        }
    }

    fun streamChat(request: ChatRequest, onEvent: (ChatEvent) -> Unit): CompletableFuture<Void> {
        val parser = SseParser()
        val body = buildString {
            append("{")
            append("\"message\":${request.message.jsonQuoted()},")
            append("\"working_dir\":${request.workingDir.jsonQuoted()},")
            append("\"session_id\":${request.sessionId.jsonQuoted()}")
            request.provider?.let { append(",\"provider\":${it.jsonQuoted()}") }
            if (request.images.isNotEmpty()) {
                append(",\"images\":[")
                request.images.forEachIndexed { index, image ->
                    if (index > 0) append(",")
                    append("{")
                    append("\"media_type\":${image.mediaType.jsonQuoted()},")
                    append("\"data\":${image.data.jsonQuoted()}")
                    append("}")
                }
                append("]")
            }
            append("}")
        }
        val httpRequest = requestBuilder("/chat")
            .header("Accept", "text/event-stream")
            .POST(HttpRequest.BodyPublishers.ofString(body))
            .build()

        // 使用 ofInputStream 代替 fromLineSubscriber：
        // fromLineSubscriber 基于 Java Reactive Streams Flow API，
        // 其内部递归调用在 SSE 高频事件场景下会触发 java.lang.StackOverflowError。
        // ofInputStream 直接读取字节流，逐行解析，无需 Flow API。
        val future = CompletableFuture<Void>()
        client.sendAsync(httpRequest, HttpResponse.BodyHandlers.ofInputStream())
            .thenAcceptAsync({ response ->
                try {
                    if (response.statusCode() >= 400) {
                        val error = response.body().bufferedReader().use { it.readText() }
                        onEvent(ChatEvent.Error("Daemon request failed: HTTP ${response.statusCode()}${error.daemonErrorSuffix()}"))
                        future.complete(null)
                        return@thenAcceptAsync
                    }
                    response.body().bufferedReader().use { reader ->
                        var line = reader.readLine()
                        while (line != null) {
                            try {
                                parser.feed("$line\n").forEach(onEvent)
                            } catch (e: Exception) {
                                onEvent(ChatEvent.Error("Parse error: ${e.message}"))
                            }
                            line = reader.readLine()
                        }
                        parser.flush().forEach(onEvent)
                    }
                } catch (e: Exception) {
                    onEvent(ChatEvent.Error("Stream read error: ${e.message}"))
                }
                future.complete(null)
            }, java.util.concurrent.CompletableFuture.delayedExecutor(0, java.util.concurrent.TimeUnit.MILLISECONDS))
            .exceptionally { error ->
                onEvent(ChatEvent.Error("Stream connection failed: ${error.cause?.message ?: error.message}"))
                future.complete(null)
                null
            }

        return future
    }

    private fun send(method: String, path: String, body: String? = null): CompletableFuture<String> {
        val builder = requestBuilder(path)
        val publisher = if (body == null) HttpRequest.BodyPublishers.noBody() else HttpRequest.BodyPublishers.ofString(body)
        val request = builder.method(method, publisher).build()

        return client.sendAsync(request, HttpResponse.BodyHandlers.ofString()).thenApply { response ->
            if (response.statusCode() >= 400) {
                throw IllegalStateException("Daemon request failed: HTTP ${response.statusCode()}")
            }
            response.body()
        }
    }

    private fun requestBuilder(path: String): HttpRequest.Builder {
        val builder = HttpRequest.newBuilder(URI.create("$baseUrl$path"))
            .timeout(Duration.ofMillis(timeoutMs.toLong()))
            .header("Content-Type", "application/json")
            .header("X-AtomCode-Client", "jetbrains")

        auth.token?.takeIf { it.isNotBlank() }?.let {
            builder.header("Authorization", "Bearer $it")
        }
        return builder
    }
}

internal fun String.jsonQuoted(): String =
    "\"" + replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n") + "\""

private fun String.daemonErrorSuffix(): String {
    val message = jsonString("error") ?: jsonString("message") ?: trim()
    return message.takeIf { it.isNotBlank() }?.let { ": $it" }.orEmpty()
}

internal fun String?.jsonQuotedOrNull(): String = this?.jsonQuoted() ?: "null"

internal fun String.urlPathEncoded(): String =
    java.net.URLEncoder.encode(this, Charsets.UTF_8).replace("+", "%20")

internal fun String.urlQueryEncoded(): String =
    java.net.URLEncoder.encode(this, Charsets.UTF_8)

internal fun parseSessionMetaList(raw: String): List<SessionMeta> =
    raw.jsonObjects().map {
        val meta = it.jsonNestedObject("meta") ?: it
        SessionMeta(
            id = meta.jsonString("id").orEmpty(),
            name = meta.jsonString("name").orEmpty(),
            projectHash = it.jsonString("project_hash") ?: meta.jsonString("project_hash").orEmpty(),
            updatedAt = meta.jsonLong("updated_at") ?: 0L,
            messageCount = meta.jsonInt("message_count") ?: 0,
        )
    }.filter { it.id.isNotBlank() }

private fun String.toProviderInfo(): ProviderInfo =
    ProviderInfo(
        name = jsonString("name").orEmpty(),
        type = jsonString("type").orEmpty(),
        model = jsonString("model").orEmpty(),
        isDefault = jsonBoolean("is_default") ?: false,
        hasApiKey = jsonBoolean("has_api_key") ?: false,
        thinkingEnabled = jsonBoolean("thinking_enabled") ?: false,
        thinkingBudget = jsonInt("thinking_budget"),
        thinkingType = jsonString("thinking_type"),
        thinkingKeep = jsonString("thinking_keep"),
    )
