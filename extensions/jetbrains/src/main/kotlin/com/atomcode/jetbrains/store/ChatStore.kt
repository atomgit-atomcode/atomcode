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

    fun setApprovalMode(mode: ApprovalMode) {
        val currentState = _state.value
        if (currentState.pendingApprovalMode != null || currentState.approvalMode == mode.wire) return
        _state.update {
            it.copy(
                approvalMode = mode.wire,
                pendingApprovalMode = mode.wire,
            )
        }
        scope.launch {
            runCatching { client.setApprovalMode(mode) }
                .onSuccess { response ->
                    val applied = response.mode.ifBlank { mode.wire }
                    _state.update { current ->
                        if (current.pendingApprovalMode == mode.wire) {
                            current.copy(
                                approvalMode = applied,
                                confirmedApprovalMode = applied,
                                pendingApprovalMode = null,
                            )
                        } else {
                            current
                        }
                    }
                }
                .onFailure {
                    _state.update { current ->
                        if (current.pendingApprovalMode == mode.wire) {
                            current.copy(
                                approvalMode = current.confirmedApprovalMode,
                                pendingApprovalMode = null,
                            )
                        } else {
                            current
                        }
                    }
                }
            drainQueueIfReady()
        }
    }

    fun submitPrompt(
        text: String,
        images: List<ImageRef> = emptyList(),
        contextFiles: List<String> = emptyList(),
        sessionId: String? = null,
        workingDir: String? = null
    ) {
        val current = _state.value
        if (current.pendingApprovalMode != null ||
            current.generation is GenerationState.Streaming ||
            current.generation is GenerationState.WaitingPermission) {
            enqueuePrompt(text, images, contextFiles, sessionId, workingDir, current.confirmedApprovalMode)
            return
        }

        startPrompt(text, images, contextFiles, sessionId, workingDir, current.confirmedApprovalMode)
    }

    private fun enqueuePrompt(
        text: String,
        images: List<ImageRef>,
        contextFiles: List<String>,
        sessionId: String?,
        workingDir: String?,
        approvalMode: String,
    ) {
        _state.update {
            it.copy(
                queue = it.queue.toMutableList()
                    .apply {
                        add(
                            QueuedPrompt(
                                id = defaultIds("q"),
                                text = text,
                                approvalMode = approvalMode,
                                images = images,
                                contextFiles = contextFiles,
                                sessionId = sessionId,
                                workingDir = workingDir,
                            )
                        )
                    }
                    .toImmutableList()
            )
        }
    }

    private fun startPrompt(
        text: String,
        images: List<ImageRef> = emptyList(),
        contextFiles: List<String> = emptyList(),
        sessionId: String? = null,
        workingDir: String? = null,
        approvalMode: String,
    ) {
        val current = _state.value
        dispatch(ChatAction.SubmitPrompt(text, images, contextFiles))
        val sid = sessionId ?: current.sessionId

        streamJob = scope.launch {
            try {
                val stream = client.streamChat(
                    ChatRequest(message = text,
                        images = images.map { ImageInput(it.mediaType, it.data) },
                        sessionId = sid,
                        workingDir = workingDir,
                        approvalMode = approvalMode)
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
        _state.update { it.copy(generation = GenerationState.Idle) }
        drainQueueIfReady()
    }

    private fun drainQueueIfReady() {
        val current = _state.value
        if (current.generation !is GenerationState.Idle ||
            current.pendingApprovalMode != null ||
            current.queue.isEmpty()) {
            return
        }

        if (current.queue.isNotEmpty()) {
            val next = current.queue[0]
            _state.update { it.copy(queue = it.queue.toMutableList().apply { removeAt(0) }.toImmutableList()) }
            startPrompt(
                text = next.text,
                images = next.images,
                contextFiles = next.contextFiles,
                sessionId = next.sessionId,
                workingDir = next.workingDir,
                approvalMode = next.approvalMode,
            )
        }
    }
}
