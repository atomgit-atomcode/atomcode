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
    private data class TurnSummary(
        val label: String,
        val rounds: Int,
        val toolCalls: Int,
        val duration: String,
        val tokens: Int,
        val failed: Boolean,
    )

    /** AI 是否已开始输出（收到过 Text/Reasoning 事件） */
    var hasOutput: Boolean = false
        private set

    /** AI 文本输出累积 */
    var assistantText: String = ""
        private set

    /** 当前工具事件之间的文本段；用于保持“文本 → 工具 → 文本”的展示顺序。 */
    private var assistantSegmentText: String = ""

    private var activeArtifactId: String? = null
    private var activeArtifactText: String = ""
    private var activeArtifactAssistantPrefix: String = ""
    private var activeArtifactLanguage: String = "text"
    private var activeArtifactTitle: String? = null

    /** AI 思考过程累积 */
    var reasoningText: String = ""
        private set

    private var activeToolName: String? = null
    private var activeToolOutput: String = ""
    private var activeToolSummary: String = ""
    private var turnStartedAtNanos: Long = System.nanoTime()
    private var turnSummaryShown: Boolean = false
    private var lastTurnSummary: TurnSummary? = null

    // ── Event handlers ──

    fun onText(content: String) {
        appendAssistantMarkdown(content)
    }

    fun onReasoning(content: String) {
        reasoningText += content
        messageView.updateReasoningBlock(reasoningText)
    }

    fun onToolBatch() {
        flushAssistantSegment()
        assistantSegmentText = ""
        messageView.hideStreamingCursor()
        messageView.addAssistantEvent("[Tools queued]")
    }

    fun onToolStart(name: String, arguments: String) {
        flushAssistantSegment()
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

    fun onArtifactStart(id: String, artifactType: String, language: String?, title: String?) {
        clearActiveArtifact()
        flushAssistantSegment()
        assistantSegmentText = ""
        activeArtifactId = id
        activeArtifactText = ""
        val fenceLanguage = artifactLanguage(artifactType, language, title)
        activeArtifactAssistantPrefix = assistantText
        activeArtifactLanguage = fenceLanguage
        activeArtifactTitle = title
        ensureAssistantOutputStarted()
        assistantText = activeArtifactAssistantPrefix + fencedArtifactMarkdown(fenceLanguage, activeArtifactText)
        messageView.updateArtifactCodeBlock(id, fenceLanguage, title, activeArtifactText)
        messageView.showStreamingCursor()
    }

    fun onArtifactContent(id: String, content: String) {
        if (content.isEmpty()) return
        if (activeArtifactId != id) return

        activeArtifactText = appendArtifactContent(activeArtifactText, content)
        assistantText = activeArtifactAssistantPrefix + fencedArtifactMarkdown(activeArtifactLanguage, activeArtifactText)
        messageView.updateArtifactCodeBlock(id, activeArtifactLanguage, activeArtifactTitle, activeArtifactText)
        messageView.showStreamingCursor()
    }

    fun onArtifactEnd(id: String) {
        if (activeArtifactId == null || activeArtifactId == id) {
            clearActiveArtifact()
            assistantSegmentText = ""
        }
    }

    fun onPermissionRequired(event: ChatEvent.PermissionRequest) {
        messageView.addAssistantEvent("[Permission required] ${event.toolName}: ${event.reason}")
    }

    fun onStopped() {
        flushAssistantSegment()
        messageView.finishAssistantTurn()
        messageView.addAssistantEvent("[Stopped]")
        addTurnSummary("Stopped", tokens = 0, toolCalls = 0, failed = true)
    }

    fun onError(message: String) {
        flushAssistantSegment()
        messageView.finishAssistantTurn()
        messageView.addError(message)
        addTurnSummary("Error", tokens = 0, toolCalls = 0, failed = true)
        hasOutput = true
    }

    fun onWarning(message: String) {
        messageView.addAssistantEvent("[Warning] $message")
        hasOutput = true
    }

    fun onUnknown(type: String) {
        messageView.addAssistantEvent("[Unknown event] $type")
        hasOutput = true
    }

    fun onDone(tokens: Int, toolCalls: Int) {
        flushAssistantSegment()
        messageView.finishAssistantTurn()
        addTurnSummary("Dialed in", tokens, toolCalls, failed = false)
    }

    /** 流完成时收尾：如果没有输出，清理思考指示器 */
    fun onComplete() {
        flushAssistantSegment()
        messageView.finishAssistantTurn()
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("(no output)")
        }
        addTurnSummary("Dialed in", tokens = 0, toolCalls = 0, failed = false)
    }

    /** 重置状态，准备新一轮对话 */
    fun reset() {
        hasOutput = false
        assistantText = ""
        assistantSegmentText = ""
        activeArtifactId = null
        activeArtifactText = ""
        activeArtifactAssistantPrefix = ""
        activeArtifactLanguage = "text"
        activeArtifactTitle = null
        reasoningText = ""
        activeToolName = null
        activeToolOutput = ""
        activeToolSummary = ""
        turnStartedAtNanos = System.nanoTime()
        turnSummaryShown = false
        lastTurnSummary = null
    }

    fun replayLastTurnSummary() {
        val summary = lastTurnSummary ?: return
        messageView.addTurnSummary(
            label = summary.label,
            rounds = summary.rounds,
            toolCalls = summary.toolCalls,
            duration = summary.duration,
            tokens = summary.tokens,
            failed = summary.failed,
        )
    }

    private fun addTurnSummary(label: String, tokens: Int, toolCalls: Int, failed: Boolean) {
        if (turnSummaryShown) return
        turnSummaryShown = true
        val summary = TurnSummary(
            label = label,
            rounds = 1,
            toolCalls = toolCalls.coerceAtLeast(0),
            duration = formatDuration(System.nanoTime() - turnStartedAtNanos),
            tokens = tokens.coerceAtLeast(0),
            failed = failed,
        )
        lastTurnSummary = summary
        messageView.addTurnSummary(
            label = summary.label,
            rounds = summary.rounds,
            toolCalls = summary.toolCalls,
            duration = summary.duration,
            tokens = summary.tokens,
            failed = summary.failed,
        )
    }

    private fun flushAssistantSegment() {
        if (assistantSegmentText.isEmpty()) {
            messageView.finishAssistantMarkdownStream()
        } else {
            messageView.finishAssistantMarkdownStream(assistantSegmentText)
        }
    }

    private fun appendAssistantMarkdown(content: String) {
        assistantText += content
        assistantSegmentText += content
        ensureAssistantOutputStarted()
        messageView.appendAssistantDelta(content)
        messageView.showStreamingCursor()
    }

    private fun ensureAssistantOutputStarted() {
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("")
            hasOutput = true
        }
    }

    private fun clearActiveArtifact() {
        activeArtifactId = null
        activeArtifactText = ""
        activeArtifactAssistantPrefix = ""
        activeArtifactLanguage = "text"
        activeArtifactTitle = null
    }

}

internal fun appendArtifactContent(
    current: String,
    incoming: String,
): String = current + incoming

internal fun fencedArtifactMarkdown(language: String, content: String): String {
    val maxBacktickRun = Regex("`+").findAll(content)
        .maxOfOrNull { it.value.length }
        ?: 0
    val fence = "`".repeat(maxOf(3, maxBacktickRun + 1))
    return buildString {
        append(fence)
        append(language)
        append('\n')
        append(content)
        if (content.isNotEmpty() && !content.endsWith("\n")) {
            append('\n')
        }
        append(fence)
        append('\n')
    }
}

internal fun artifactLanguage(artifactType: String, language: String?, title: String?): String {
    val candidates = listOf(language, artifactType, title)
    return candidates.asSequence()
        .filterNot { it.isNullOrBlank() }
        .mapNotNull { candidate ->
            val cleaned = candidate
                ?.trim()
                ?.takeUnless { it.any { ch -> ch == '\n' || ch == '\r' || ch == '`' || ch == '~' } }
            val token = cleaned
                ?.split(Regex("[ \t]+"))
                ?.firstOrNull()
                ?.ifBlank { null }
            token?.takeIf { SAFE_ARTIFACT_LANGUAGE.matches(it) }
        }
        .firstOrNull()
        ?: "text"
}

private val SAFE_ARTIFACT_LANGUAGE = Regex("[A-Za-z0-9][A-Za-z0-9_+.#-]*")

private fun formatDuration(nanos: Long): String {
    val millis = (nanos / 1_000_000).coerceAtLeast(0)
    return if (millis < 1_000) {
        "${millis}ms"
    } else {
        "%.1fs".format(java.util.Locale.ROOT, millis / 1_000.0)
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
