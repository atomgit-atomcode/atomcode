package com.atomcode.jetbrains.ui.message

import com.atomcode.jetbrains.session.AssistantStatus
import com.atomcode.jetbrains.session.ChatState
import com.atomcode.jetbrains.session.MessageState
import com.atomcode.jetbrains.session.SystemLevel
import com.atomcode.jetbrains.session.ToolStatus

data class ChatRenderModel(
    val version: Int = 1,
    val tabId: String,
    val messages: List<MessageRenderModel>,
)

sealed interface MessageRenderModel {
    val id: String

    data class User(
        override val id: String,
        val text: String,
        val contextSummary: List<String>,
    ) : MessageRenderModel

    data class Assistant(
        override val id: String,
        val markdown: String,
        val reasoning: String?,
        val status: AssistantStatus,
    ) : MessageRenderModel

    data class ToolCall(
        override val id: String,
        val callId: String,
        val name: String,
        val argumentsJson: String,
        val output: String,
        val status: ToolStatus,
        val durationMs: Long?,
    ) : MessageRenderModel

    data class Permission(
        override val id: String,
        val requestId: String,
        val toolName: String,
        val reason: String,
        val arguments: String,
    ) : MessageRenderModel

    data class System(
        override val id: String,
        val text: String,
        val level: SystemLevel,
    ) : MessageRenderModel
}

fun ChatState.toRenderModel(): ChatRenderModel =
    ChatRenderModel(
        tabId = tabId,
        messages = messages.map { it.toRenderModel() },
    )

private fun MessageState.toRenderModel(): MessageRenderModel =
    when (this) {
        is MessageState.User -> MessageRenderModel.User(id, text, contextSummary)
        is MessageState.Assistant -> MessageRenderModel.Assistant(id, rawMarkdown, reasoning, status)
        is MessageState.ToolCall -> MessageRenderModel.ToolCall(
            id = id,
            callId = callId,
            name = name,
            argumentsJson = argumentsJson,
            output = outputChunks.joinToString(""),
            status = status,
            durationMs = durationMs,
        )
        is MessageState.Permission -> MessageRenderModel.Permission(
            id = id,
            requestId = request.requestId,
            toolName = request.toolName,
            reason = request.reason,
            arguments = request.arguments,
        )
        is MessageState.System -> MessageRenderModel.System(id, text, level)
    }
