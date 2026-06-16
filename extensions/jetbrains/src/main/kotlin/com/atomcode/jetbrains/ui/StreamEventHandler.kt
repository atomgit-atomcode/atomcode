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

    // ── Event handlers ──

    fun onText(content: String) {
        assistantText += content
        if (!hasOutput) {
            // 首次正文：如果有思考过程先展示，再替换思考指示器
            if (reasoningText.isNotBlank()) {
                messageView.addReasoningBlock(reasoningText)
            }
            messageView.replaceThinkingWithAssistant(assistantText)
            hasOutput = true
        } else {
            messageView.updateLastAssistantMessage(assistantText)
        }
    }

    fun onReasoning(content: String) {
        reasoningText += content
        // 思考内容仅累积，不替换思考指示器
    }

    fun onToolBatch() {
        messageView.addSystemMessage("[Tools queued]")
    }

    fun onToolStart(name: String) {
        messageView.addToolCall(name, "running...")
    }

    fun onToolResult(name: String, success: Boolean, durationMs: Long) {
        val status = if (success) "done (${durationMs}ms)" else "failed"
        messageView.updateToolCall(name, status)
    }

    fun onArtifactStart(title: String?) {
        messageView.addSystemMessage("[Artifact] ${title ?: "untitled"} started")
    }

    fun onArtifactContent(content: String) {
        assistantText += content
        if (hasOutput) {
            messageView.updateLastAssistantMessage(assistantText)
        }
    }

    fun onArtifactEnd(id: String) {
        messageView.addSystemMessage("[Artifact] $id ended")
    }

    fun onPermissionRequired(event: ChatEvent.PermissionRequest) {
        messageView.addSystemMessage("[Permission required] ${event.toolName}: ${event.reason}")
    }

    fun onStopped() {
        messageView.addSystemMessage("[Stopped]")
    }

    fun onError(message: String) {
        messageView.addError(message)
    }

    fun onUnknown(type: String) {
        messageView.addSystemMessage("[Unknown event] $type")
    }

    /** 流完成时收尾：如果没有输出，清理思考指示器 */
    fun onComplete() {
        if (!hasOutput) {
            messageView.replaceThinkingWithAssistant("(no output)")
        }
    }

    /** 重置状态，准备新一轮对话 */
    fun reset() {
        hasOutput = false
        assistantText = ""
        reasoningText = ""
    }
}
