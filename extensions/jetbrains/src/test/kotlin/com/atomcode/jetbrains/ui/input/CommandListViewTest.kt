package com.atomcode.jetbrains.ui.input

import java.awt.event.MouseEvent
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * 选择器的交互面：↑/↓ 环绕移动、高亮越界收敛、点击行回调插入。
 * 这些都不需要弹窗真的显示，所以可以在无窗口环境里钉住。
 */
class CommandListViewTest {
    private val commands = listOf(
        SlashCommand("/login", "登录 AtomGit"),
        SlashCommand("/logout", "退出 AtomGit 登录"),
        SlashCommand("/status", "显示会话状态"),
    )

    @Test
    fun `moving down wraps around and up wraps back`() {
        val view = CommandListView {}.apply { items = commands }

        assertEquals(0, view.selectedIndex)
        view.move(1); assertEquals(1, view.selectedIndex)
        view.move(1); assertEquals(2, view.selectedIndex)
        view.move(1); assertEquals(0, view.selectedIndex, "末行再往下回到首行")
        view.move(-1); assertEquals(2, view.selectedIndex, "首行再往上回到末行")
    }

    @Test
    fun `the selection stays inside the list when it shrinks`() {
        val view = CommandListView {}.apply { items = commands }
        view.selectedIndex = 2

        view.items = commands.take(1)

        assertEquals(0, view.selectedIndex)
    }

    @Test
    fun `a point maps to the row under it`() {
        val view = CommandListView {}.apply { items = commands }

        assertEquals(0, view.indexAt(java.awt.Point(5, 0)))
        assertEquals(1, view.indexAt(java.awt.Point(5, SlashCommandPopup.ROW_HEIGHT + 2)))
        assertNull(view.indexAt(java.awt.Point(5, -1)), "输入框上方的空白不算行")
        assertNull(view.indexAt(java.awt.Point(5, SlashCommandPopup.ROW_HEIGHT * 99)))
    }

    @Test
    fun `clicking a row inserts that command`() {
        val picked = mutableListOf<String>()
        val view = CommandListView { picked += it.name }.apply { items = commands }

        click(view, row = 1)

        assertEquals(listOf("/logout"), picked)
        assertEquals(1, view.selectedIndex)
    }

    @Test
    fun `the preferred height follows the row count`() {
        val view = CommandListView {}.apply { items = commands }

        assertEquals(commands.size * SlashCommandPopup.ROW_HEIGHT, view.preferredSize.height)
        // 原始宽度可能比下限还小，实际使用的宽度会被夹到 MIN_WIDTH。
        assertTrue(view.preferredSize.width >= SlashCommandPopup.MIN_WIDTH)
        assertTrue(view.preferredWidth() > 0)
    }

    private fun click(view: CommandListView, row: Int) {
        val y = row * SlashCommandPopup.ROW_HEIGHT + SlashCommandPopup.ROW_HEIGHT / 2
        listOf(MouseEvent.MOUSE_PRESSED, MouseEvent.MOUSE_RELEASED).forEach { id ->
            view.dispatchEvent(
                MouseEvent(
                    view,
                    id,
                    System.currentTimeMillis(),
                    0,
                    1,
                    y,
                    1,
                    false,
                    MouseEvent.BUTTON1,
                ),
            )
        }
    }
}
