package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.services.SessionRefView
import java.util.UUID
import java.util.concurrent.CopyOnWriteArrayList

typealias ChatStateListener = (ChatState) -> Unit

class ChatStateStore(
    initialState: ChatState,
    private val ids: IdFactory = IdFactory.uuid(),
    private val clock: Clock = Clock.system(),
) {
    private val listeners = CopyOnWriteArrayList<ChatStateListener>()

    @Volatile
    var state: ChatState = initialState
        private set

    fun dispatch(action: ChatAction): ChatState {
        val next = reduce(state, action, ids, clock)
        state = next
        listeners.forEach { it(next) }
        return next
    }

    fun subscribe(listener: ChatStateListener): AutoCloseable {
        listeners += listener
        listener(state)
        return AutoCloseable { listeners -= listener }
    }
}

fun interface IdFactory {
    fun next(prefix: String): String

    companion object {
        fun uuid(): IdFactory = IdFactory { prefix -> "$prefix-${UUID.randomUUID()}" }
    }
}

fun interface Clock {
    fun nowMillis(): Long

    companion object {
        fun system(): Clock = Clock { System.currentTimeMillis() }
    }
}

internal fun reduce(
    state: ChatState,
    action: ChatAction,
    ids: IdFactory,
    clock: Clock,
): ChatState =
    when (action) {
        is ChatAction.SubmitPrompt -> state.submitPrompt(action.text, ids, clock)
        is ChatAction.QueuePrompt -> state.queuePrompt(action.text, action.id, ids, clock)
        is ChatAction.RemoveQueuedPrompt -> state.copy(queue = state.queue.filterNot { it.id == action.id })
        is ChatAction.AddContext -> state.copy(pendingContext = state.pendingContext + action.item)
        is ChatAction.RemoveContext -> state.copy(pendingContext = state.pendingContext.filterNot { it.id == action.id })
        ChatAction.ClearContext -> state.copy(pendingContext = emptyList())
        is ChatAction.DraftChanged -> state.copy(draft = action.text)
        is ChatAction.SessionRefUpdated -> state.copy(session = action.session)
        is ChatAction.SessionLoaded -> state.loadSession(action.detail, clock)
        is ChatAction.ConnectionChanged -> state.copy(connection = action.state)
        is ChatAction.DaemonEventReceived -> state.applyDaemonEvent(action.event, ids, clock)
        is ChatAction.PermissionDecision -> state.applyPermissionDecision(action.requestId)
        ChatAction.StopRequested -> state.copy(generation = GenerationState.Stopping)
    }

private fun ChatState.submitPrompt(text: String, ids: IdFactory, clock: Clock): ChatState {
    val trimmed = text.trim()
    if (trimmed.isEmpty()) return this
    val contextSummary = pendingContext.map { item ->
        if (item.selectionStartLine != null && item.selectionEndLine != null) {
            "${item.displayName}:${item.selectionStartLine}-${item.selectionEndLine}"
        } else {
            item.displayName
        }
    }
    val user = MessageState.User(
        id = ids.next("user"),
        createdAt = clock.nowMillis(),
        text = trimmed,
        contextSummary = contextSummary,
    )
    return copy(
        messages = messages + user,
        pendingContext = emptyList(),
        draft = "",
        generation = GenerationState.Streaming(assistantMessageId = ids.next("assistant")),
    )
}

private fun ChatState.queuePrompt(text: String, id: String?, ids: IdFactory, clock: Clock): ChatState {
    val trimmed = text.trim()
    if (trimmed.isEmpty()) return this
    return copy(
        queue = queue + QueuedPromptState(
            id = id ?: ids.next("queue"),
            text = trimmed,
            contextSummary = pendingContext.map { it.displayName },
            createdAt = clock.nowMillis(),
        ),
        draft = "",
    )
}

private fun ChatState.loadSession(detail: com.atomcode.jetbrains.daemon.SessionDetail, clock: Clock): ChatState {
    val loadedMessages = detail.messages.mapIndexed { index, message ->
        when (message.role) {
            "user" -> MessageState.User(
                id = "history-user-$index",
                createdAt = clock.nowMillis(),
                text = message.content,
                contextSummary = emptyList(),
            )
            "assistant" -> MessageState.Assistant(
                id = "history-assistant-$index",
                createdAt = clock.nowMillis(),
                rawMarkdown = message.content,
                status = AssistantStatus.Complete,
                reasoning = null,
            )
            else -> MessageState.System(
                id = "history-system-$index",
                createdAt = clock.nowMillis(),
                text = "${message.role}: ${message.content}",
                level = SystemLevel.Info,
            )
        }
    }
    return copy(
        session = SessionRefView(detail.id, detail.name, detail.projectHash, detail.workingDir),
        messages = loadedMessages,
        generation = GenerationState.Idle,
        activePermission = null,
        queue = emptyList(),
    )
}

private fun ChatState.applyDaemonEvent(event: ChatEvent, ids: IdFactory, clock: Clock): ChatState =
    when (event) {
        is ChatEvent.Text -> appendAssistantText(event.content, ids, clock)
        is ChatEvent.Reasoning -> appendReasoning(event.content, ids, clock)
        is ChatEvent.ToolBatch -> addSystemMessage("[Tools queued]", SystemLevel.Info, ids, clock)
        is ChatEvent.ToolStart -> addToolCall(event, ids, clock)
        is ChatEvent.ToolOutput -> appendToolOutput(event.chunk)
        is ChatEvent.ToolResult -> completeToolCall(event, ids, clock)
        is ChatEvent.ArtifactStart -> addSystemMessage("[Artifact] ${event.title ?: event.id} started", SystemLevel.Info, ids, clock)
        is ChatEvent.ArtifactContent -> this
        is ChatEvent.ArtifactEnd -> addSystemMessage("[Artifact] ${event.id} ended", SystemLevel.Info, ids, clock)
        is ChatEvent.PermissionRequest -> addPermissionRequest(event, ids, clock)
        is ChatEvent.Tokens -> this
        is ChatEvent.Warning -> addSystemMessage(event.message, SystemLevel.Warning, ids, clock)
        is ChatEvent.Done -> completeGeneration(event.sessionId)
        ChatEvent.Stopped -> copy(generation = GenerationState.Idle)
            .addSystemMessage("[Stopped]", SystemLevel.Warning, ids, clock)
        is ChatEvent.Error -> copy(generation = GenerationState.Failed(event.message))
            .addSystemMessage(event.message, SystemLevel.Error, ids, clock)
        is ChatEvent.Unknown -> addSystemMessage("[Unknown event] ${event.type}", SystemLevel.Warning, ids, clock)
    }

private fun ChatState.appendAssistantText(content: String, ids: IdFactory, clock: Clock): ChatState {
    if (content.isEmpty()) return this
    val existingIndex = messages.indexOfLast { it is MessageState.Assistant && it.status == AssistantStatus.Streaming }
    val nextMessages = if (existingIndex >= 0) {
        messages.replaceAt(existingIndex) { message ->
            val assistant = message as MessageState.Assistant
            assistant.copy(rawMarkdown = assistant.rawMarkdown + content)
        }
    } else {
        messages + MessageState.Assistant(
            id = ids.next("assistant"),
            createdAt = clock.nowMillis(),
            rawMarkdown = content,
            status = AssistantStatus.Streaming,
            reasoning = null,
        )
    }
    return copy(messages = nextMessages)
}

private fun ChatState.appendReasoning(content: String, ids: IdFactory, clock: Clock): ChatState {
    if (content.isEmpty()) return this
    val existingIndex = messages.indexOfLast { it is MessageState.Assistant && it.status == AssistantStatus.Streaming }
    val nextMessages = if (existingIndex >= 0) {
        messages.replaceAt(existingIndex) { message ->
            val assistant = message as MessageState.Assistant
            assistant.copy(reasoning = assistant.reasoning.orEmpty() + content)
        }
    } else {
        messages + MessageState.Assistant(
            id = ids.next("assistant"),
            createdAt = clock.nowMillis(),
            rawMarkdown = "",
            status = AssistantStatus.Streaming,
            reasoning = content,
        )
    }
    return copy(messages = nextMessages)
}

private fun ChatState.addToolCall(event: ChatEvent.ToolStart, ids: IdFactory, clock: Clock): ChatState {
    val callId = event.id ?: ids.next("tool-call")
    val tool = MessageState.ToolCall(
        id = ids.next("tool"),
        createdAt = clock.nowMillis(),
        callId = callId,
        name = event.name,
        argumentsJson = event.arguments,
        outputChunks = emptyList(),
        status = ToolStatus.Running,
        durationMs = null,
    )
    return copy(messages = messages + tool)
}

private fun ChatState.appendToolOutput(chunk: String): ChatState {
    if (chunk.isEmpty()) return this
    val index = messages.indexOfLast { it is MessageState.ToolCall && it.status == ToolStatus.Running }
    if (index < 0) return this
    return copy(messages = messages.replaceAt(index) { message ->
        val tool = message as MessageState.ToolCall
        tool.copy(outputChunks = tool.outputChunks + chunk)
    })
}

private fun ChatState.completeToolCall(event: ChatEvent.ToolResult, ids: IdFactory, clock: Clock): ChatState {
    val index = messages.indexOfLast {
        it is MessageState.ToolCall &&
            (event.id == null || it.callId == event.id || it.name == event.name)
    }
    if (index < 0) {
        return addToolCall(
            ChatEvent.ToolStart(event.id, event.name, ""),
            ids,
            clock,
        ).completeToolCall(event, ids, clock)
    }
    return copy(messages = messages.replaceAt(index) { message ->
        val tool = message as MessageState.ToolCall
        tool.copy(
            outputChunks = if (event.output.isBlank()) tool.outputChunks else tool.outputChunks + event.output,
            status = if (event.success) ToolStatus.Success else ToolStatus.Failed,
            durationMs = event.durationMs,
        )
    })
}

private fun ChatState.addPermissionRequest(event: ChatEvent.PermissionRequest, ids: IdFactory, clock: Clock): ChatState {
    val request = PermissionRequestState(
        requestId = ids.next("permission"),
        sessionId = event.sessionId,
        toolName = event.toolName,
        reason = event.reason,
        callId = event.callId,
        arguments = event.arguments,
    )
    val message = MessageState.Permission(
        id = ids.next("permission-message"),
        createdAt = clock.nowMillis(),
        request = request,
    )
    return copy(
        messages = messages + message,
        activePermission = request,
        generation = GenerationState.WaitingPermission(request.requestId),
    )
}

private fun ChatState.applyPermissionDecision(requestId: String): ChatState {
    if (activePermission?.requestId != requestId) return this
    return copy(
        activePermission = null,
        generation = GenerationState.Streaming(assistantMessageId = idsFromStreamingGeneration()),
    )
}

private fun ChatState.completeGeneration(sessionId: String?): ChatState {
    val completedMessages = messages.map { message ->
        if (message is MessageState.Assistant && message.status == AssistantStatus.Streaming) {
            message.copy(status = AssistantStatus.Complete)
        } else {
            message
        }
    }
    val nextSession = if (sessionId != null && session == null) {
        SessionRefView(sessionId, "AtomCode Chat", "", "")
    } else {
        session
    }
    return copy(messages = completedMessages, session = nextSession, generation = GenerationState.Idle)
}

private fun ChatState.addSystemMessage(
    text: String,
    level: SystemLevel,
    ids: IdFactory,
    clock: Clock,
): ChatState =
    copy(messages = messages + MessageState.System(ids.next("system"), clock.nowMillis(), text, level))

private fun ChatState.idsFromStreamingGeneration(): String =
    (generation as? GenerationState.Streaming)?.assistantMessageId ?: "assistant-active"

private inline fun <T> List<T>.replaceAt(index: Int, transform: (T) -> T): List<T> =
    mapIndexed { currentIndex, value -> if (currentIndex == index) transform(value) else value }
