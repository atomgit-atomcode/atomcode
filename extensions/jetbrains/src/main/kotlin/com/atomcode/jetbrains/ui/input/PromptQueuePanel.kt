package com.atomcode.jetbrains.ui.input

import com.intellij.ui.JBColor
import java.awt.FlowLayout
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
            BorderFactory.createEmptyBorder(3, 8, 3, 8),
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
        add(JLabel("Queue:").apply {
            font = font.deriveFont(font.size2D - 2f)
            foreground = JBColor(0x7A5A16, 0xC9A85C)
        })

        items.forEachIndexed { index, item ->
            val chip = JPanel(FlowLayout(FlowLayout.LEFT, 2, 0)).apply {
                background = CHIP_BG
                border = BorderFactory.createCompoundBorder(
                    BorderFactory.createLineBorder(CHIP_BORDER, 1, true),
                    BorderFactory.createEmptyBorder(1, 6, 1, 2),
                )
            }
            chip.add(JLabel("${index + 1}. ${item.text.compactPromptLabel()}").apply {
                font = font.deriveFont(font.size2D - 2f)
                foreground = CHIP_FG
            })
            if (item.contextSummary.isNotEmpty()) {
                chip.add(JLabel("(${item.contextSummary.size})").apply {
                    font = font.deriveFont(font.size2D - 3f)
                    foreground = JBColor(0x8A6A28, 0xB99B5F)
                })
            }
            chip.add(JButton("x").apply {
                font = font.deriveFont(java.awt.Font.BOLD, font.size2D)
                isContentAreaFilled = false
                isBorderPainted = false
                isFocusPainted = false
                foreground = JBColor(0x7A5A16, 0xC9A85C)
                addActionListener { onRemove(item) }
            })
            add(chip)
        }

        revalidate()
        repaint()
    }

    companion object {
        private val QUEUE_BG = JBColor(0xF7F1E4, 0x2A2518)
        private val QUEUE_BORDER = JBColor(0xD7C59A, 0x4A3E24)
        private val CHIP_BG = JBColor(0xFFF7E4, 0x3A2D16)
        private val CHIP_BORDER = JBColor(0xE1C071, 0x6A5122)
        private val CHIP_FG = JBColor(0x624A12, 0xE2C27B)
    }
}

private fun String.compactPromptLabel(): String {
    val singleLine = lineSequence().joinToString(" ").trim()
    return if (singleLine.length <= 80) singleLine else singleLine.take(77) + "..."
}
