package com.atomcode.jetbrains.ui.input

import java.awt.BorderLayout
import java.awt.GraphicsEnvironment
import javax.swing.JFrame
import javax.swing.JPanel
import javax.swing.JTextArea
import javax.swing.SwingUtilities
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * 弹窗几何：底边必须压在输入框上沿（不能飘远、也不能压住输入框），高度不超过
 * 输入框上方的可用空间。需要有显示环境，headless 下跳过。
 */
class SlashCommandPopupAnchorTest {
    private fun onEdt(block: () -> Unit) {
        val t = Thread { SwingUtilities.invokeAndWait(block) }
        t.start()
        t.join()
    }

    @Test
    fun `the popup sits on the input's top edge and stays on screen`() {
        if (GraphicsEnvironment.isHeadless()) return
        val commands = (1..120).map {
            SlashCommand("/skill-$it-name", "description for skill $it")
        } + SlashCommands.BUILT_INS
        onEdt {
            val frame = JFrame("popup-anchor")
            val field = JTextArea(3, 30)
            val composer = JPanel(BorderLayout()).apply { add(field, BorderLayout.SOUTH) }
            frame.contentPane.add(JPanel(BorderLayout()).apply { add(composer, BorderLayout.SOUTH) })
            frame.setSize(1100, 900)
            frame.isVisible = true
            try {
                val popup = SlashCommandPopup(anchor = { field }, onInsert = {})
                popup.show(commands, "/")

                val location = popup.popupLocationOnScreen()
                val inputTop = field.locationOnScreen.y
                val inputLeft = field.locationOnScreen.x
                assertTrue(location != null, "弹窗应当已显示")
                val height = popup.popupBounds().height
                assertEquals(inputTop, location!!.y + height, "底边必须正好贴住输入框上沿")
                assertTrue(location.y >= 0, "不能超出屏幕顶部")
                assertTrue(height <= inputTop, "高度不能超过输入框上方的空间")
                // 只占上方可用空间的一半左右：列表别把整段对话盖住。
                assertTrue(height <= inputTop / 2 + SlashCommandPopup.ROW_HEIGHT, "高度应当约为上方空间的一半")
                // 左沿与输入框对齐（本用例里外框就是文字区）。
                assertEquals(inputLeft, location.x, "左沿应当与输入框左沿对齐")
                // 与输入框同宽（vscode 的选择器和输入框等宽）。
                assertEquals(field.width, popup.popupBounds().width, "宽度应当与输入框一致")

                popup.hide()
            } finally {
                frame.dispose()
            }
        }
    }
}
