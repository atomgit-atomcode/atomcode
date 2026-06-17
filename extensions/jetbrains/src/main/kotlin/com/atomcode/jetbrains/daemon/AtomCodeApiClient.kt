package com.atomcode.jetbrains.daemon

import java.util.concurrent.CompletableFuture

interface AtomCodeApiClient {
    fun health(): CompletableFuture<HealthResponse>
    fun listSessions(): CompletableFuture<List<SessionMeta>>
    fun searchSessions(query: String): CompletableFuture<List<SessionMeta>>
    fun getSession(projectHash: String, sessionId: String): CompletableFuture<SessionDetail>
    fun createSession(title: String?, workingDir: String): CompletableFuture<SessionRef>
    fun streamChat(request: ChatRequest, onEvent: (ChatEvent) -> Unit): CompletableFuture<Void>
    fun sendPermissionDecision(
        sessionId: String,
        decision: String,
        toolName: String? = null,
    ): CompletableFuture<PermissionDecisionResponse>
}

class ExistingDaemonApiClient(
    private val delegate: AtomCodeDaemonClient,
) : AtomCodeApiClient {
    override fun health(): CompletableFuture<HealthResponse> = delegate.health()

    override fun listSessions(): CompletableFuture<List<SessionMeta>> = delegate.listSessions()

    override fun searchSessions(query: String): CompletableFuture<List<SessionMeta>> = delegate.searchSessions(query)

    override fun getSession(projectHash: String, sessionId: String): CompletableFuture<SessionDetail> =
        delegate.getSession(projectHash, sessionId)

    override fun createSession(title: String?, workingDir: String): CompletableFuture<SessionRef> =
        delegate.createSession(title, workingDir)

    override fun streamChat(request: ChatRequest, onEvent: (ChatEvent) -> Unit): CompletableFuture<Void> =
        delegate.streamChat(request, onEvent)

    override fun sendPermissionDecision(
        sessionId: String,
        decision: String,
        toolName: String?,
    ): CompletableFuture<PermissionDecisionResponse> =
        delegate.sendPermissionDecision(sessionId, decision, toolName)
}
