package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.ui.ChatContextItem
import com.intellij.ui.JBColor
import java.awt.FlowLayout
import javax.swing.BorderFactory
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JPanel

/**
 * 上下文文件标签面板，显示已附加的文件 chips + 清除按钮。
 */
class ContextChipsPanel(
    private val onClear: () -> Unit,
) : JPanel(FlowLayout(FlowLayout.LEFT, 4, 2)) {

    init {
        isOpaque = true
        // JBColor(亮色, 暗色)
        background = JBColor(0xF0F0F0, 0x252525)
        border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(1, 0, 0, 0, JBColor(0xCCCCCC, 0x333333)),
            BorderFactory.createEmptyBorder(3, 8, 3, 8),
        )
        isVisible = false
    }

    fun setItems(items: List<ChatContextItem>, onRemove: (ChatContextItem) -> Unit) {
        removeAll()
        if (items.isEmpty()) {
            isVisible = false
            return
        }
        isVisible = true

        add(JLabel("上下文:").apply {
            font = font.deriveFont(font.size2D - 2f)
            // JBColor(亮色, 暗色)
            foreground = JBColor(0x666666, 0x888888)
        })

        items.forEach { item ->
            val chip = JPanel(FlowLayout(FlowLayout.LEFT, 2, 0)).apply {
                background = CHIP_BG
                border = BorderFactory.createCompoundBorder(
                    BorderFactory.createLineBorder(CHIP_BORDER, 1, true),
                    BorderFactory.createEmptyBorder(1, 6, 1, 2),
                )
            }
            val label = buildString {
                append("📄 ")
                append(item.displayName)
                if (item.startLine != null) {
                    append(" (L${item.startLine}-${item.endLine})")
                }
            }
            chip.add(JLabel(label).apply {
                font = font.deriveFont(font.size2D - 2f)
                foreground = CHIP_FG
            })
            chip.add(JButton("×").apply {
                font = font.deriveFont(java.awt.Font.BOLD, font.size2D + 1f)
                isContentAreaFilled = false
                isBorderPainted = false
                isFocusPainted = false
                // JBColor(亮色, 暗色)
                foreground = JBColor(0x666666, 0x888888)
                addActionListener { onRemove(item) }
            })
            add(chip)
        }

        add(JButton("清除").apply {
            font = font.deriveFont(font.size2D - 2f)
            isContentAreaFilled = false
            isBorderPainted = false
            isFocusPainted = false
            // JBColor(亮色, 暗色)
            foreground = JBColor(0x666666, 0x888888)
            addActionListener { onClear() }
        })

        revalidate()
        repaint()
    }

    companion object {
        // JBColor(亮色, 暗色)
        private val CHIP_BG = JBColor(0xD0E4F7, 0x1E3A5F)
        private val CHIP_BORDER = JBColor(0xA0C0E0, 0x264F78)
        private val CHIP_FG = JBColor(0x005A9E, 0x9CDCFE)
    }
}
