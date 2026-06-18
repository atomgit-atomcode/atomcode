package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.protocol.CreateSessionResponse
import com.atomcode.jetbrains.protocol.SessionMeta
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class SessionStore(
    private val client: DaemonClient,
    private val scope: CoroutineScope
) {
    private val _sessions = MutableStateFlow<List<SessionMeta>>(emptyList())
    val sessions: StateFlow<List<SessionMeta>> = _sessions.asStateFlow()

    fun refresh() {
        scope.launch {
            try { _sessions.value = client.listSessions() } catch (_: Exception) {}
        }
    }

    suspend fun create(workingDir: String, title: String? = null): CreateSessionResponse {
        val response = client.createSession(workingDir, title)
        refresh()
        return response
    }

    suspend fun delete(projectHash: String, sessionId: String) {
        client.deleteSession(projectHash, sessionId)
        refresh()
    }

    suspend fun rename(projectHash: String, sessionId: String, name: String) {
        client.renameSession(projectHash, sessionId, name)
        refresh()
    }

    suspend fun search(query: String): List<SessionMeta> = client.searchSessions(query)
}
