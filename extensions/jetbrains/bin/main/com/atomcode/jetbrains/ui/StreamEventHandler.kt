package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.ui.message.JBCefMessageView
import com.google.gson.JsonParser

/**
 * 流式事件集中处理器。
 *
 * 替代 startPrompt() 中的匿名 ChatStreamListener，
 * 集中管理消息状态和 UI 更新逻辑。
 */
class StreamEventHandler(
    private val messageView: JBCefMessageView,
) {
    /** AI 是否已开始输出（收到过 Text/Reasoning 事件） */
    var hasOutput: Boolean = false
        private set

    /** AI 文本输出累积 */
    var assistantText: String = ""
        private set

    /** 当前工具事件之间的文本段；用于保持“文本 → 工具 → 文本”的展示顺序。 */
    private var assistantSegmentText: String = ""

    /** AI 思考过程累积 */
    var reasoningText: String = ""
        private set

    private var activeToolName: String? = null
    private var activeToolOutput: String = ""
    private var activeToolSummary: String = ""

    // ── Event handlers ──

    fun onText(content: String) {
        assistantText += content
        assistantSegmentText += content
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("")
            messageView.updateLastAssistantMessage(assistantSegmentText)
            messageView.showStreamingCursor()
            hasOutput = true
        } else {
            messageView.updateLastAssistantMessage(assistantSegmentText)
            messageView.showStreamingCursor()
        }
    }

    fun onReasoning(content: String) {
        reasoningText += content
        messageView.updateReasoningBlock(reasoningText)
    }

    fun onToolBatch() {
        assistantSegmentText = ""
        messageView.hideStreamingCursor()
        messageView.addAssistantEvent("[Tools queued]")
    }

    fun onToolStart(name: String, arguments: String) {
        assistantSegmentText = ""
        messageView.hideStreamingCursor()
        activeToolName = name
        activeToolOutput = ""
        activeToolSummary = summarizeToolArguments(name, arguments)
        messageView.addToolCall(name, "running...", summary = activeToolSummary)
    }

    fun onToolOutput(chunk: String) {
        if (chunk.isEmpty()) return
        val name = activeToolName ?: "tool"
        activeToolName = name
        activeToolOutput += chunk
        val status = "running... (${activeToolOutput.length} chars)"
        messageView.updateToolCall(name, status, activeToolOutput, activeToolSummary)
    }

    fun onToolResult(name: String, output: String, success: Boolean, durationMs: Long) {
        val status = if (success) "done (${durationMs}ms)" else "failed"
        val detail = output.ifBlank { activeToolOutput }
        messageView.updateToolCall(name, status, detail, activeToolSummary)
        activeToolName = null
        activeToolOutput = ""
        activeToolSummary = ""
    }

    fun onArtifactStart(title: String?) {
        messageView.addAssistantEvent("[Artifact] ${title ?: "untitled"} started")
    }

    fun onArtifactContent(content: String) {
        // Artifacts are rendered as separate daemon events. Do not append them to
        // assistantText here, otherwise final assistant content can be duplicated.
    }

    fun onArtifactEnd(id: String) {
        messageView.addAssistantEvent("[Artifact] $id ended")
    }

    fun onPermissionRequired(event: ChatEvent.PermissionRequest) {
        messageView.addAssistantEvent("[Permission required] ${event.toolName}: ${event.reason}")
    }

    fun onStopped() {
        messageView.finishAssistantTurn()
        messageView.addAssistantEvent("[Stopped]")
    }

    fun onError(message: String) {
        messageView.finishAssistantTurn()
        messageView.addError(message)
    }

    fun onUnknown(type: String) {
        messageView.addAssistantEvent("[Unknown event] $type")
    }

    /** 流完成时收尾：如果没有输出，清理思考指示器 */
    fun onComplete() {
        messageView.finishAssistantTurn()
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("(no output)")
        }
    }

    /** 重置状态，准备新一轮对话 */
    fun reset() {
        hasOutput = false
        assistantText = ""
        assistantSegmentText = ""
        reasoningText = ""
        activeToolName = null
        activeToolOutput = ""
        activeToolSummary = ""
    }
}

internal fun summarizeToolArguments(name: String, arguments: String): String {
    val args = try {
        JsonParser.parseString(arguments).takeIf { it.isJsonObject }?.asJsonObject ?: return ""
    } catch (_: Exception) {
        return ""
    }

    fun string(vararg keys: String): String = keys.firstNotNullOfOrNull { key ->
        args.get(key)?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
    }.orEmpty()

    val summary = when (name.lowercase()) {
        "bash", "execute_command" -> string("command", "cmd")
        "read_file", "create_file", "edit_file", "write_to_file", "replace_in_file" ->
            string("file_path", "path")
        "list_directory" -> string("path").ifBlank { "." }
        "grep", "search_files" -> listOf(string("pattern", "query"), string("path"))
            .filter { it.isNotBlank() }
            .joinToString("  ·  ")
        "glob" -> string("pattern")
        "web_search" -> string("query")
        "web_fetch" -> string("url")
        else -> ""
    }

    val singleLine = summary.lineSequence().joinToString(" ") { it.trim() }
        .replace(Regex("\\s+"), " ")
        .trim()
    return if (singleLine.length <= 120) singleLine else singleLine.take(117) + "..."
}
