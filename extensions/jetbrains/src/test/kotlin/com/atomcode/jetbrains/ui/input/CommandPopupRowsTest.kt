package com.atomcode.jetbrains.ui.input

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * 命令弹窗的行数上限：列表高过屏幕时 Swing 会把它渲染成重型窗口并自己抢焦点，
 * 随后把焦点抢回输入框就把菜单关了——用户看到的是"列表闪一下就不见"。
 * 所以行数必须截断到屏幕放得下，并留一行提示还有多少项没显示。
 */
class CommandPopupRowsTest {
    @Test
    fun `rows are capped so the popup never exceeds the screen`() {
        // 1512x982 的屏幕实测：24px 一行时整份列表约 1338px，必须截断到上限。
        assertEquals(MAX_COMMAND_ROWS, commandRowsThatFit(availableHeight = 982, rowHeight = 24))
        // 屏幕更矮时按实际能放下的行数走（(400-160)/24 = 10）。
        assertEquals(10, commandRowsThatFit(availableHeight = 400, rowHeight = 24))
        // 极矮的窗口也不能缩到无法操作。
        assertEquals(MIN_COMMAND_ROWS, commandRowsThatFit(availableHeight = 100, rowHeight = 24))
    }

    @Test
    fun `a degenerate row height still produces a usable window`() {
        assertTrue(commandRowsThatFit(availableHeight = 900, rowHeight = 0) >= MIN_COMMAND_ROWS)
    }
}
