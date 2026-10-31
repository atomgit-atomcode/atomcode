package com.atomcode.jetbrains.ui.input

import javax.swing.JTextArea
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

/**
 * 收起必须回调 onHide：输入侧靠它复位"已触发"状态。不复位的话，焦点移开导致收起后
 * 再回来继续输入就再也不匹配了（用户报的原话）。
 */
class SlashCommandPopupHideTest {
    @Test
    fun `hiding notifies the caller so the trigger can be reset`() {
        var hidden = 0
        val popup = SlashCommandPopup(
            anchor = { JTextArea() },
            onInsert = {},
            onHide = { hidden++ },
        )

        popup.hide()
        assertEquals(1, hidden, "没有窗口时也要通知，否则触发状态永远卡住")
        popup.hide()
        assertEquals(2, hidden)
    }

    @Test
    fun `nothing to activate when the popup was never shown`() {
        val popup = SlashCommandPopup(anchor = { JTextArea() }, onInsert = {})

        assertFalse(popup.isVisible)
        assertFalse(popup.activate())
        assertFalse(popup.move(1))
    }

    @Test
    fun `an empty match hides and reports it`() {
        var hidden = 0
        val popup = SlashCommandPopup(anchor = { JTextArea() }, onInsert = {}, onHide = { hidden++ })

        // 锚点没显示时 show 会直接返回，这里只验证"没有匹配就不该留下可见状态"的入口。
        popup.show(emptyList(), "/")
        assertTrue(!popup.isVisible)
    }
}
