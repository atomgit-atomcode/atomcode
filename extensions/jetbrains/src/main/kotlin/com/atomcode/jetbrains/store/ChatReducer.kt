package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.protocol.*
import kotlinx.collections.immutable.ImmutableList
import kotlinx.collections.immutable.persistentListOf
import kotlinx.collections.immutable.toImmutableList
import java.util.UUID

typealias IdGenerator = (prefix: String) -> String

val defaultIds: IdGenerator = { prefix -> "$prefix-${UUID.randomUUID()}" }

// 纯函数，零依赖，零 IO
fun reduce(state: ChatState, action: ChatAction, ids: IdGenerator = defaultIds): ChatState {
    return when (action) {
        is ChatAction.SubmitPrompt -> state.handleSubmitPrompt(action, ids)
        is ChatAction.DaemonEvent -> state.handleDaemonEvent(action.event, ids)
        is ChatAction.StopGeneration -> state.copy(generation = GenerationState.Stopping)
        is ChatAction.PermissionDecision -> state.handlePermissionDecision(action, ids)
        is ChatAction.LoadSession -> state.handleLoadSession(action)
    }
}

private fun ChatState.handleSubmitPrompt(action: ChatAction.SubmitPrompt, ids: IdGenerator): ChatState {
    val userMsg = Message.User(
        id = ids("u"), text = action.text, images = action.images, contextFiles = action.contextFiles,
    )
    val assistantId = ids("a")
    return copy(
        messages = messages.toMutableList().apply { add(userMsg); add(Message.Assistant(id = assistantId)) }.toImmutableList(),
        generation = GenerationState.Streaming(assistantId),
        pendingPermission = null, tokens = null, draft = "",
    )
}

private fun ChatState.handleDaemonEvent(event: ChatEvent, ids: IdGenerator): ChatState {
    return when (event) {
        is ChatEvent.Text -> appendText(event.content)
        is ChatEvent.Reasoning -> appendReasoning(event.content)
        is ChatEvent.ToolBatchStarted -> addToolCalls(event.calls)
        is ChatEvent.ToolCallStarted -> markToolRunning(event.id)
        is ChatEvent.ToolOutputChunk -> appendToolOutput(event)
        is ChatEvent.ToolCallResult -> finishTool(event)
        is ChatEvent.ArtifactStart -> startArtifact(event)
        is ChatEvent.ArtifactContent -> appendArtifactContent(event)
        is ChatEvent.ArtifactEnd -> finishArtifact(event.id)
        is ChatEvent.PermissionRequest -> setPendingPermission(event)
        is ChatEvent.TokenUsage -> copy(tokens = TokenUsageState(event.prompt, event.completion, event.total))
        is ChatEvent.Done -> finishGeneration(event)
        is ChatEvent.Stopped -> copy(generation = GenerationState.Idle)
        is ChatEvent.Error -> copy(
            generation = GenerationState.Failed(event.message),
        ).updateLastAssistant { it.copy(status = AssistantStatus.Complete) }
    }
}

private fun ChatState.appendText(content: String): ChatState =
    updateLastAssistant { it.copy(markdown = it.markdown + content) }

private fun ChatState.appendReasoning(content: String): ChatState =
    updateLastAssistant { it.copy(reasoning = it.reasoning + content) }

private fun ChatState.addToolCalls(calls: List<ToolBatchCall>): ChatState =
    updateLastAssistant { assistant ->
        assistant.copy(toolCalls = assistant.toolCalls.toMutableList().apply {
            addAll(calls.map { ToolCallEntry(it.id, it.name, it.arguments, status = ToolStatus.Queued) })
        }.toImmutableList())
    }

private fun ChatState.markToolRunning(callId: String): ChatState =
    updateLastAssistant { it.copy(
        toolCalls = it.toolCalls.map { tc ->
            if (tc.callId == callId) tc.copy(status = ToolStatus.Running) else tc
        }.toImmutableList()
    )}

private fun ChatState.appendToolOutput(event: ChatEvent.ToolOutputChunk): ChatState =
    updateLastAssistant { assistant ->
        val tools = assistant.toolCalls.toMutableList()
        val idx = tools.indexOfLast { it.callId == event.id && it.status == ToolStatus.Running }
        if (idx >= 0) tools[idx] = tools[idx].copy(output = tools[idx].output + event.chunk)
        assistant.copy(toolCalls = tools.toImmutableList())
    }

private fun ChatState.finishTool(result: ChatEvent.ToolCallResult): ChatState =
    updateLastAssistant { it.copy(
        toolCalls = it.toolCalls.map { tc ->
            if (tc.callId == result.id) tc.copy(
                output = result.output, success = result.success,
                durationMs = result.duration_ms,
                status = if (result.success) ToolStatus.Success else ToolStatus.Error,
            ) else tc
        }.toImmutableList()
    )}

private fun ChatState.startArtifact(event: ChatEvent.ArtifactStart): ChatState =
    updateLastAssistant { it.copy(
        artifacts = it.artifacts.toMutableList().apply { add(ArtifactEntry(
            id = event.id, artifactType = event.artifact_type,
            language = event.language, title = event.title,
        )) }.toImmutableList()
    )}

private fun ChatState.appendArtifactContent(event: ChatEvent.ArtifactContent): ChatState =
    updateLastAssistant { it.copy(
        artifacts = it.artifacts.map { a ->
            if (a.id == event.id) a.copy(content = a.content + event.content, status = ArtifactStatus.Streaming) else a
        }.toImmutableList()
    )}

private fun ChatState.finishArtifact(artifactId: String): ChatState =
    updateLastAssistant { it.copy(
        artifacts = it.artifacts.map { a ->
            if (a.id == artifactId) a.copy(status = ArtifactStatus.Complete) else a
        }.toImmutableList()
    )}

private fun ChatState.setPendingPermission(event: ChatEvent.PermissionRequest): ChatState =
    copy(
        pendingPermission = PermissionRequestState(
            sessionId = event.session_id, toolName = event.tool_name,
            reason = event.reason, callId = event.call_id, arguments = event.arguments,
            severity = PermissionSeverityState.Destructive, // severity 来自 daemon 元数据，未来用 event.severity 映射
            allowPersist = event.allow_persist ?: false,
        ),
        generation = GenerationState.WaitingPermission(event.call_id),
    )

private fun ChatState.finishGeneration(event: ChatEvent.Done): ChatState =
    copy(
        sessionId = event.session_id ?: sessionId,
        generation = GenerationState.Idle,
        tokens = (tokens ?: TokenUsageState(0, 0, 0)).copy(total = event.tokens),
    ).updateLastAssistant { it.copy(status = AssistantStatus.Complete) }

private fun ChatState.handlePermissionDecision(action: ChatAction.PermissionDecision, ids: IdGenerator): ChatState =
    copy(
        pendingPermission = null,
        generation = GenerationState.Streaming(ids("a-resume")),
    )

private fun ChatState.handleLoadSession(action: ChatAction.LoadSession): ChatState {
    val msgs = action.detail.messages.mapIndexed { index, info ->
        when (info.role) {
            "user" -> Message.User(
                id = "hist-u-$index", text = info.content,
                images = info.images?.map { ImageRef(it.mediaType, it.data) } ?: emptyList(),
            )
            "assistant" -> Message.Assistant(
                id = "hist-a-$index", markdown = info.content,
                status = AssistantStatus.Complete,
                toolCalls = info.toolCalls?.map { tc ->
                    ToolCallEntry(tc.id, tc.name, tc.arguments, status = ToolStatus.Success)
                }?.toImmutableList() ?: persistentListOf(),
                artifacts = info.artifacts?.map { a ->
                    ArtifactEntry(a.id, a.artifactType, a.title, a.language, a.content, ArtifactStatus.Complete)
                }?.toImmutableList() ?: persistentListOf(),
            )
            else -> Message.User(id = "hist-$index", text = info.content)
        }
    }
    return copy(sessionId = action.detail.id, messages = msgs.toImmutableList(), generation = GenerationState.Idle)
}

// ── Helpers ──

private fun ChatState.updateLastAssistant(transform: (Message.Assistant) -> Message.Assistant): ChatState {
    val msgs = messages.toMutableList()
    for (i in msgs.lastIndex downTo 0) {
        if (msgs[i] is Message.Assistant) {
            msgs[i] = transform(msgs[i] as Message.Assistant)
            return copy(messages = msgs.toImmutableList())
        }
    }
    return this
}
