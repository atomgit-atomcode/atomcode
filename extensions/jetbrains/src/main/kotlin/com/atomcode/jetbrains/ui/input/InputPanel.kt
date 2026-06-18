package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.ui.ChatContextItem
import com.intellij.ui.JBColor
import java.awt.BorderLayout
import java.awt.Dimension
import java.awt.FlowLayout
import javax.swing.BoxLayout
import javax.swing.AbstractAction
import javax.swing.BorderFactory
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.KeyStroke

/**
 * 输入区域容器：ContextChips + 输入行 + 底部工具栏。Claude Code 风格。
 */
class InputPanel(
    private val onSend: (String) -> Unit,
    private val onStop: () -> Unit,
    private val onAttach: () -> Unit,
    private val onSlashCommand: () -> Unit,
    private val onClearContext: () -> Unit,
    private val onRemoveContext: (ChatContextItem) -> Unit,
    private val onModelSelect: () -> Unit,
) : JPanel(BorderLayout()) {

    private val inputArea = JTextArea().apply {
        rows = 3
        lineWrap = true
        wrapStyleWord = true
        background = INPUT_BG
        foreground = INPUT_FG
        caretColor = INPUT_FG
        border = BorderFactory.createEmptyBorder(6, 10, 6, 10)
        font = font.deriveFont(font.size2D)
    }

    private val sendButton = JButton("↑ 发送").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
        background = SEND_BG
        // JBColor(亮色, 暗色) — 白色文字在两个主题下都适用
        foreground = JBColor(0xFFFFFF, 0xFFFFFF)
        isFocusPainted = false
        border = BorderFactory.createEmptyBorder(6, 14, 6, 14)
        isOpaque = true
        addActionListener { fireSend() }
    }

    private val stopButton = JButton("⏹ 停止").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
        background = STOP_BG
        foreground = JBColor(0xFFFFFF, 0xFFFFFF)
        isFocusPainted = false
        isOpaque = true
        border = BorderFactory.createEmptyBorder(6, 14, 6, 14)
        isVisible = false
        addActionListener { onStop() }
    }

    private val contextChips = ContextChipsPanel(onClear = onClearContext)
    private val queueChips = PromptQueuePanel()

    private val tokenLabel = JLabel("").apply {
        font = font.deriveFont(font.size2D - 2f)
        // JBColor(亮色, 暗色)
        foreground = JBColor(0x999999, 0x666666)
    }

    private val modelLabel = JLabel("GPT-4o ▾").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 2f)
        // JBColor(亮色, 暗色)
        foreground = JBColor(0x2D8A6E, 0x4EC9B0)
        cursor = java.awt.Cursor.getPredefinedCursor(java.awt.Cursor.HAND_CURSOR)
        addMouseListener(object : java.awt.event.MouseAdapter() {
            override fun mouseClicked(e: java.awt.event.MouseEvent) = onModelSelect()
        })
    }

    init {
        isOpaque = true
        // JBColor(亮色, 暗色)
        background = JBColor(0xF5F5F5, 0x1E1E1E)

        val inputScroll = JScrollPane(inputArea).apply {
            // JBColor(亮色, 暗色)
            border = BorderFactory.createCompoundBorder(
                BorderFactory.createLineBorder(JBColor(0xBBBBBB, 0x444444), 1),
                BorderFactory.createEmptyBorder(0, 0, 0, 0),
            )
            preferredSize = Dimension(200, 58)
            minimumSize = Dimension(100, 40)
            horizontalScrollBarPolicy = JScrollPane.HORIZONTAL_SCROLLBAR_NEVER
            verticalScrollBarPolicy = JScrollPane.VERTICAL_SCROLLBAR_NEVER
        }

        val buttonPanel = JPanel(BorderLayout(0, 4)).apply {
            isOpaque = false
            add(sendButton, BorderLayout.NORTH)
            add(stopButton, BorderLayout.SOUTH)
        }

        val inputRow = JPanel(BorderLayout(6, 0)).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(6, 8, 0, 8)
            add(inputScroll, BorderLayout.CENTER)
            add(buttonPanel, BorderLayout.EAST)
        }

        // 底部工具栏：左(附件/命令) + 右(提示/token/model)
        val toolbar = JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(2, 10, 6, 10)
            val left = JPanel(FlowLayout(FlowLayout.LEFT, 6, 0)).apply {
                isOpaque = false
                add(makeToolButton("📎 附件", onAttach))
                add(makeToolButton("⚡ /命令", onSlashCommand))
            }
            val right = JPanel(FlowLayout(FlowLayout.RIGHT, 8, 0)).apply {
                isOpaque = false
                add(JLabel("Enter 发送 · Shift+Enter 换行").apply {
                    font = font.deriveFont(font.size2D - 2f)
                    foreground = JBColor(0xAAAAAA, 0x555555)
                })
                add(tokenLabel)
                add(modelLabel)
            }
            add(left, BorderLayout.WEST)
            add(right, BorderLayout.EAST)
        }

        val chips = JPanel().apply {
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            isOpaque = false
            add(queueChips)
            add(contextChips)
        }

        add(chips, BorderLayout.NORTH)
        add(inputRow, BorderLayout.CENTER)
        add(toolbar, BorderLayout.SOUTH)
    }

    fun getInputText(): String = inputArea.text

    fun setInputText(text: String) {
        inputArea.text = text
    }

    fun focusInput() {
        inputArea.requestFocusInWindow()
    }

    fun setGenerating(generating: Boolean) {
        sendButton.isVisible = !generating
        stopButton.isVisible = generating
        sendButton.text = if (generating) "排队中" else "↑ 发送"
    }

    fun setContextItems(items: List<ChatContextItem>) {
        contextChips.setItems(items) { item -> onRemoveContext(item) }
    }

    fun setQueuedPrompts(items: List<QueuedPromptView>, onRemove: (QueuedPromptView) -> Unit) {
        queueChips.setItems(items, onRemove)
    }

    fun setModelName(name: String) {
        modelLabel.text = "$name ▾"
    }

    fun setTokenCount(current: Int, max: Int) {
        tokenLabel.text = if (max > 0) "$current/$max" else ""
    }

    fun clearInput() {
        inputArea.text = ""
    }

    fun installKeyBindings(sendWithCtrlEnter: Boolean) {
        val enterAction = "atomcode-input-enter"
        val ctrlEnterAction = "atomcode-input-ctrl-enter"

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ENTER"), enterAction)
        inputArea.actionMap.put(enterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (sendWithCtrlEnter) {
                    inputArea.append("\n")
                } else {
                    fireSend()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ctrl ENTER"), ctrlEnterAction)
        inputArea.actionMap.put(ctrlEnterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (sendWithCtrlEnter) {
                    fireSend()
                } else {
                    inputArea.append("\n")
                }
            }
        })
    }

    private fun fireSend() {
        val text = inputArea.text.trim()
        if (text.isNotEmpty()) {
            inputArea.text = ""
            onSend(text)
        }
    }

    private fun makeToolButton(text: String, action: () -> Unit): JButton =
        JButton(text).apply {
            font = font.deriveFont(font.size2D - 2f)
            isContentAreaFilled = false
            isBorderPainted = false
            isFocusPainted = false
            // JBColor(亮色, 暗色)
            foreground = JBColor(0x666666, 0x999999)
            addActionListener { action() }
        }

    companion object {
        // JBColor(亮色, 暗色)
        private val INPUT_BG = JBColor(0xFFFFFF, 0x2D2D2D)
        private val INPUT_FG = JBColor(0x333333, 0xD4D4D4)
        private val SEND_BG = JBColor(0x0078D4, 0x0E639C)
        private val STOP_BG = JBColor(0xC04040, 0xA03030)
    }
}
