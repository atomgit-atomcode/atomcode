package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.protocol.*
import kotlinx.collections.immutable.toImmutableList
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.launch

class ChatStore(
    val tabId: String,
    private val client: DaemonClient,
    private val scope: CoroutineScope
) {
    private val _state = MutableStateFlow(ChatState(tabId = tabId))
    val state: StateFlow<ChatState> = _state.asStateFlow()
    private var streamJob: Job? = null

    fun dispatch(action: ChatAction) {
        _state.update { reduce(it, action, defaultIds) }
    }

    fun submitPrompt(
        text: String,
        images: List<ImageRef> = emptyList(),
        contextFiles: List<String> = emptyList(),
        sessionId: String? = null,
        workingDir: String? = null
    ) {
        val current = _state.value
        if (current.generation is GenerationState.Streaming ||
            current.generation is GenerationState.WaitingPermission) {
            _state.update { it.copy(queue = it.queue.toMutableList().apply { add(QueuedPrompt(defaultIds("q"), text)) }.toImmutableList()) }
            return
        }

        dispatch(ChatAction.SubmitPrompt(text, images, contextFiles))
        val sid = sessionId ?: current.sessionId

        streamJob = scope.launch {
            try {
                val stream = client.streamChat(
                    ChatRequest(message = text,
                        images = images.map { ImageInput(it.mediaType, it.data) },
                        sessionId = sid,
                        workingDir = workingDir)
                )
                stream.events().collect { event ->
                    dispatch(ChatAction.DaemonEvent(event))
                }
                afterStreamComplete()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                dispatch(ChatAction.DaemonEvent(ChatEvent.Error(e.message ?: "Unknown error")))
            }
        }
    }

    fun stop() {
        streamJob?.cancel()
        dispatch(ChatAction.StopGeneration)
        scope.launch {
            _state.value.sessionId?.let { client.stopChat(it) }
        }
    }

    fun submitPermission(callId: String, decision: PermissionDecisionKind) {
        dispatch(ChatAction.PermissionDecision(callId, decision))
        scope.launch {
            val current = _state.value
            if (current.sessionId != null) {
                client.submitPermissionDecision(PermissionDecisionRequest(
                    sessionId = current.sessionId,
                    decision = when (decision) {
                        PermissionDecisionKind.Allow -> "allow"
                        PermissionDecisionKind.Deny -> "deny"
                        PermissionDecisionKind.AlwaysAllow -> "always_allow"
                        PermissionDecisionKind.AllowPersist -> "allow_persist"
                    },
                    toolName = current.pendingPermission?.toolName,
                ))
            }
        }
    }

    suspend fun restoreSession(sessionId: String, projectHash: String) {
        val detail = client.getSessionDetail(projectHash, sessionId)
        dispatch(ChatAction.LoadSession(detail))
    }

    private fun afterStreamComplete() {
        val current = _state.value
        _state.update { it.copy(generation = GenerationState.Idle) }
        if (current.queue.isNotEmpty()) {
            val next = current.queue[0]
            _state.update { it.copy(queue = it.queue.toMutableList().apply { removeAt(0) }.toImmutableList()) }
            submitPrompt(next.text)
        }
    }
}
