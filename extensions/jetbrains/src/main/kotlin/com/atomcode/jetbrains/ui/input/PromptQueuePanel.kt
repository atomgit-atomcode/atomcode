package com.atomcode.jetbrains.ui.input

import com.intellij.ui.JBColor
import java.awt.FlowLayout
import java.awt.Font
import javax.swing.BorderFactory
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JPanel

data class QueuedPromptView(
    val id: String,
    val text: String,
    val contextSummary: List<String>,
)

class PromptQueuePanel : JPanel(FlowLayout(FlowLayout.LEFT, 4, 2)) {
    private var initialized = false

    init {
        isOpaque = true
        isVisible = false
        initialized = true
        applyTheme()
    }

    override fun updateUI() {
        super.updateUI()
        if (initialized) applyTheme()
    }

    fun applyTheme() {
        background = QUEUE_BG
        border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(1, 0, 0, 0, QUEUE_BORDER),
            BorderFactory.createEmptyBorder(5, 10, 5, 10),
        )
        revalidate()
        repaint()
    }

    fun setItems(items: List<QueuedPromptView>, onRemove: (QueuedPromptView) -> Unit) {
        removeAll()
        if (items.isEmpty()) {
            isVisible = false
            revalidate()
            repaint()
            return
        }

        isVisible = true
        add(JLabel("Queue ${items.size}").apply {
            font = font.deriveFont(Font.BOLD, font.size2D - 2f)
            foreground = LABEL_FG
            border = BorderFactory.createEmptyBorder(0, 0, 0, 4)
        })

        items.forEachIndexed { index, item ->
            val chip = JPanel(FlowLayout(FlowLayout.LEFT, 5, 0)).apply {
                isOpaque = true
                background = CHIP_BG
                border = BorderFactory.createCompoundBorder(
                    BorderFactory.createLineBorder(CHIP_BORDER, 1, true),
                    BorderFactory.createEmptyBorder(2, 7, 2, 3),
                )
            }
            chip.add(JLabel("${index + 1}").apply {
                font = font.deriveFont(Font.BOLD, font.size2D - 3f)
                foreground = INDEX_FG
            })
            chip.add(JLabel(item.text.compactPromptLabel()).apply {
                font = font.deriveFont(font.size2D - 2f)
                foreground = CHIP_FG
            })
            if (item.contextSummary.isNotEmpty()) {
                chip.add(JLabel("(${item.contextSummary.size})").apply {
                    font = font.deriveFont(font.size2D - 3f)
                    foreground = META_FG
                })
            }
            chip.add(JButton("×").apply {
                font = font.deriveFont(Font.BOLD, font.size2D - 1f)
                isContentAreaFilled = false
                isBorderPainted = false
                isFocusPainted = false
                foreground = REMOVE_FG
                border = BorderFactory.createEmptyBorder(0, 5, 0, 3)
                toolTipText = "Remove from queue"
                addActionListener { onRemove(item) }
            })
            add(chip)
        }

        revalidate()
        repaint()
    }

    companion object {
        private val QUEUE_BG = JBColor(0xF5F6F7, 0x25282C)
        private val QUEUE_BORDER = JBColor(0xD9DDE2, 0x3A3F45)
        private val LABEL_FG = JBColor(0x5A616A, 0xA5ABB3)
        private val CHIP_BG = JBColor(0xFFFFFF, 0x2E3338)
        private val CHIP_BORDER = JBColor(0xDDE3EA, 0x454B53)
        private val INDEX_FG = JBColor(0x7A828C, 0x9BA3AD)
        private val CHIP_FG = JBColor(0x2F343A, 0xDADDE1)
        private val META_FG = JBColor(0x747B84, 0x929AA3)
        private val REMOVE_FG = JBColor(0x7C838C, 0xA2A8AF)
    }
}

private fun String.compactPromptLabel(): String {
    val singleLine = lineSequence().joinToString(" ").trim()
    return if (singleLine.length <= 80) singleLine else singleLine.take(77) + "..."
}
