package com.atomcode.jetbrains.ui.input

import com.intellij.ui.JBColor
import java.awt.BasicStroke
import java.awt.Color
import java.awt.Component
import java.awt.Graphics
import java.awt.Graphics2D
import java.awt.Insets
import java.awt.RenderingHints
import javax.swing.border.AbstractBorder

/**
 * 圆角细线边框：底部三个选项控件（模式 / 推理强度 / 模型）共用。
 *
 * 自带内边距（原来的 EmptyBorder 值），所以换上来不会改变控件的文字位置；线宽 1px、
 * 圆角 8px。右上角那点内缩由 [arc] 决定，弹窗右对齐时以边框右沿为准。
 */
internal class RoundedLineBorder(
    private val color: Color = LINE,
    private val arc: Int = ARC,
    private val padding: Insets = Insets(5, 8, 5, 8),
) : AbstractBorder() {
    override fun paintBorder(component: Component, g: Graphics, x: Int, y: Int, width: Int, height: Int) {
        val g2 = g.create() as Graphics2D
        try {
            g2.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            g2.color = color
            g2.stroke = BasicStroke(1f)
            // 半像素内缩，1px 线不会被组件边界裁掉一半。
            g2.drawRoundRect(x, y, width - 1, height - 1, arc, arc)
        } finally {
            g2.dispose()
        }
    }

    override fun getBorderInsets(component: Component?, insets: Insets): Insets {
        insets.set(padding.top, padding.left, padding.bottom, padding.right)
        return insets
    }

    override fun isBorderOpaque(): Boolean = false

    companion object {
        const val ARC = 8

        /// 跟随主题的细线色：浅色下淡灰，深色下比分隔线稍亮。
        val LINE: Color = JBColor(Color(0xD2, 0xD7, 0xDE), Color(0x3E, 0x44, 0x4E))
    }
}
