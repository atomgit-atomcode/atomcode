package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.ui.ChatContextItem
import com.intellij.ui.JBColor
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.Dimension
import java.awt.FlowLayout
import java.awt.Insets
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
        border = BorderFactory.createEmptyBorder(5, 12, 5, 12)
        preferredSize = Dimension(72, 30)
        isOpaque = true
        addActionListener { fireSend() }
    }

    private val stopButton = JButton("⏹ 停止").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
        background = STOP_BG
        foreground = STOP_FG
        isFocusPainted = false
        isOpaque = true
        border = BorderFactory.createEmptyBorder(5, 12, 5, 12)
        preferredSize = Dimension(72, 30)
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

    private val shortcutLabel = JLabel("Enter 发送 · Shift+Enter 换行").apply {
        font = font.deriveFont(font.size2D - 2f)
        foreground = SECONDARY_FG
    }

    private val actionCards = CardLayout()
    private val actionPanel = JPanel(actionCards).apply {
        isOpaque = false
        add(sendButton, SEND_CARD)
        add(stopButton, STOP_CARD)
        preferredSize = Dimension(72, 30)
    }

    init {
        isOpaque = true
        // JBColor(亮色, 暗色)
        background = JBColor(0xF5F5F5, 0x1E1E1E)

        val inputScroll = JScrollPane(inputArea).apply {
            border = BorderFactory.createEmptyBorder()
            isOpaque = false
            viewport.isOpaque = false
            preferredSize = Dimension(200, 68)
            minimumSize = Dimension(100, 48)
            horizontalScrollBarPolicy = JScrollPane.HORIZONTAL_SCROLLBAR_NEVER
            verticalScrollBarPolicy = JScrollPane.VERTICAL_SCROLLBAR_NEVER
        }

        // 工具栏与输入框放在同一个 composer 容器内，状态切换时布局保持稳定。
        val toolbar = JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(5, 2, 1, 2)
            val left = JPanel(FlowLayout(FlowLayout.LEFT, 2, 0)).apply {
                isOpaque = false
                add(makeToolButton("附件", onAttach))
                add(makeToolButton("/ 命令", onSlashCommand))
            }
            val right = JPanel(FlowLayout(FlowLayout.RIGHT, 10, 0)).apply {
                isOpaque = false
                add(shortcutLabel)
                add(tokenLabel)
                add(modelLabel)
                add(actionPanel)
            }
            add(left, BorderLayout.WEST)
            add(right, BorderLayout.EAST)
        }

        val composer = JPanel(BorderLayout()).apply {
            isOpaque = true
            background = INPUT_BG
            border = BorderFactory.createCompoundBorder(
                BorderFactory.createLineBorder(COMPOSER_BORDER, 1, true),
                BorderFactory.createEmptyBorder(5, 9, 7, 7),
            )
            add(inputScroll, BorderLayout.CENTER)
            add(toolbar, BorderLayout.SOUTH)
        }

        val composerInset = JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(8, 10, 10, 10)
            add(composer, BorderLayout.CENTER)
        }

        val chips = JPanel().apply {
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            isOpaque = false
            add(queueChips)
            add(contextChips)
        }

        add(chips, BorderLayout.NORTH)
        add(composerInset, BorderLayout.CENTER)
    }

    fun getInputText(): String = inputArea.text

    fun setInputText(text: String) {
        inputArea.text = text
    }

    fun focusInput() {
        inputArea.requestFocusInWindow()
    }

    fun setGenerating(generating: Boolean) {
        actionCards.show(actionPanel, if (generating) STOP_CARD else SEND_CARD)
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

        shortcutLabel.text = if (sendWithCtrlEnter) {
            "Ctrl+Enter 发送 · Enter 换行"
        } else {
            "Enter 发送 · Shift+Enter 换行"
        }

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
            margin = Insets(2, 5, 2, 5)
            // JBColor(亮色, 暗色)
            foreground = JBColor(0x666666, 0x999999)
            addActionListener { action() }
        }

    companion object {
        // JBColor(亮色, 暗色)
        private val INPUT_BG = JBColor(0xFFFFFF, 0x2D2D2D)
        private val INPUT_FG = JBColor(0x333333, 0xD4D4D4)
        private val COMPOSER_BORDER = JBColor(0xC9C9C9, 0x454545)
        private val SECONDARY_FG = JBColor(0x8A8A8A, 0x707070)
        private val SEND_BG = JBColor(0x0078D4, 0x0E639C)
        private val STOP_BG = JBColor(0xF4DEDE, 0x4A2424)
        private val STOP_FG = JBColor(0xA52D2D, 0xF48771)
        private const val SEND_CARD = "send"
        private const val STOP_CARD = "stop"
    }
}
