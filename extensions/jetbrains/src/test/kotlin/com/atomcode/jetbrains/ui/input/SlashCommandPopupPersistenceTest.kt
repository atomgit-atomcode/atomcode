package com.atomcode.jetbrains.ui.input

import java.awt.BorderLayout
import java.awt.GraphicsEnvironment
import java.awt.event.MouseEvent
import javax.swing.JFrame
import javax.swing.JPanel
import javax.swing.JTextArea
import javax.swing.SwingUtilities
import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertTrue

/**
 * 焦点离开输入框 / 弹窗时**要保留**弹窗：人可能只是去别处看一眼，回来还要重新选或
 * 接着输入。只有 Esc、选中插入、发送、前缀失效才收起。
 */
class SlashCommandPopupPersistenceTest {
    private fun onEdt(block: () -> Unit) {
        val t = Thread { SwingUtilities.invokeAndWait(block) }
        t.start()
        t.join()
    }

    @Test
    fun `clicking outside keeps the popup so the person can come back`() {
        if (GraphicsEnvironment.isHeadless()) return
        val commands = SlashCommands.BUILT_INS
        onEdt {
            val frame = JFrame("popup-persistence")
            val field = JTextArea(3, 30)
            val elsewhere = JPanel()
            val composer = JPanel(BorderLayout()).apply { add(field, BorderLayout.SOUTH) }
            frame.contentPane.add(Panel(composer, elsewhere))
            frame.setSize(900, 700)
            frame.isVisible = true
            try {
                val popup = SlashCommandPopup(anchor = { field }, onInsert = {})
                popup.show(commands, "/s")
                assertTrue(popup.isVisible, "先要弹出来")

                // 点到输入框和弹窗之外（比如消息区/编辑器）。
                elsewhere.dispatchEvent(
                    MouseEvent(
                        elsewhere,
                        MouseEvent.MOUSE_PRESSED,
                        System.currentTimeMillis(),
                        0,
                        5,
                        5,
                        1,
                        false,
                        MouseEvent.BUTTON1,
                    ),
                )

                assertTrue(popup.isVisible, "焦点离开后应当保留弹窗")

                // 回来继续输入：匹配仍在同一份清单上进行。
                popup.update("/stat")
                assertTrue(
                    popup.visibleCommands.any { it.name == "/status" },
                    "回来继续输入仍要能匹配到 /status",
                )

                popup.hide()
                assertFalse(popup.isVisible, "显式收起才消失")
            } finally {
                frame.dispose()
            }
        }
    }

    private fun Panel(vararg children: java.awt.Component): JPanel =
        JPanel(BorderLayout()).apply { children.forEach { add(it) } }
}
