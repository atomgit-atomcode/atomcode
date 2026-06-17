package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.ui.message.JBCefMessageView

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

    /** AI 思考过程累积 */
    var reasoningText: String = ""
        private set

    private var activeToolName: String? = null
    private var activeToolOutput: String = ""

    // ── Event handlers ──

    fun onText(content: String) {
        assistantText += content
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("")
            if (reasoningText.isNotBlank()) {
                messageView.addReasoningBlock(reasoningText)
            }
            messageView.updateLastAssistantMessage(assistantText)
            messageView.showStreamingCursor()
            hasOutput = true
        } else {
            messageView.updateLastAssistantMessage(assistantText)
            messageView.showStreamingCursor()
        }
    }

    fun onReasoning(content: String) {
        reasoningText += content
        // 思考内容仅累积，不替换思考指示器
    }

    fun onToolBatch() {
        messageView.addAssistantEvent("[Tools queued]")
    }

    fun onToolStart(name: String) {
        activeToolName = name
        activeToolOutput = ""
        messageView.addToolCall(name, "running...")
    }

    fun onToolOutput(chunk: String) {
        if (chunk.isEmpty()) return
        val name = activeToolName ?: "tool"
        activeToolName = name
        activeToolOutput += chunk
        val status = "running... (${activeToolOutput.length} chars)"
        messageView.updateToolCall(name, status, activeToolOutput)
    }

    fun onToolResult(name: String, output: String, success: Boolean, durationMs: Long) {
        val status = if (success) "done (${durationMs}ms)" else "failed"
        val detail = output.ifBlank { activeToolOutput }
        messageView.updateToolCall(name, status, detail)
        activeToolName = null
        activeToolOutput = ""
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
        messageView.hideStreamingCursor()
        messageView.addAssistantEvent("[Stopped]")
    }

    fun onError(message: String) {
        messageView.hideStreamingCursor()
        messageView.addError(message)
    }

    fun onUnknown(type: String) {
        messageView.addAssistantEvent("[Unknown event] $type")
    }

    /** 流完成时收尾：如果没有输出，清理思考指示器 */
    fun onComplete() {
        messageView.hideStreamingCursor()
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("(no output)")
        }
    }

    /** 重置状态，准备新一轮对话 */
    fun reset() {
        hasOutput = false
        assistantText = ""
        reasoningText = ""
        activeToolName = null
        activeToolOutput = ""
    }
}
