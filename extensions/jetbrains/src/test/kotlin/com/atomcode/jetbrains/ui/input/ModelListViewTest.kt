package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo
import java.awt.Point
import java.awt.event.MouseEvent
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * 模型选择器：一行两行展示（模型名 + provider），默认模型带 default 徽标；可滚动、
 * ↑/↓ 环绕、点击选中。这里钉住不依赖窗口的那部分。
 */
class ModelListViewTest {
    private fun model(provider: String, name: String, isDefault: Boolean = false) = ModelInfo(
        provider = provider,
        model = name,
        providerType = "openai_compat",
        isDefault = isDefault,
    )

    private val models = listOf(
        model("AtomGit-deepseek-flash", "deepseek-flash"),
        model("AtomGit-lumen-alpha", "lumen-alpha", isDefault = true),
        model("claude", "claude-opus-4-7"),
    )

    @Test
    fun `the default model is marked and others are not`() {
        assertEquals(1, models.count { it.isDefault })
        assertEquals("lumen-alpha", models.first { it.isDefault }.model)
        // 徽标文案与 VS Code 一致。
        assertEquals("default", ModelListView.DEFAULT_BADGE)
    }

    @Test
    fun `moving wraps around the list`() {
        val view = ModelListView {}.apply { models = this@ModelListViewTest.models }

        assertEquals(0, view.selectedIndex)
        view.move(1); assertEquals(1, view.selectedIndex)
        view.move(1); assertEquals(2, view.selectedIndex)
        view.move(1); assertEquals(0, view.selectedIndex, "末行再往下回到首行")
        view.move(-1); assertEquals(2, view.selectedIndex, "首行再往上回到末行")
    }

    @Test
    fun `the selection stays inside the list when it shrinks`() {
        val view = ModelListView {}.apply { models = this@ModelListViewTest.models }
        view.selectedIndex = 2

        view.models = models.take(1)

        assertEquals(0, view.selectedIndex)
    }

    @Test
    fun `a point maps to the row under it`() {
        val view = ModelListView {}.apply { models = this@ModelListViewTest.models }

        assertEquals(0, view.indexAt(Point(5, 0)))
        assertEquals(1, view.indexAt(Point(5, ModelPickerPopup.ROW_HEIGHT + 2)))
        assertNull(view.indexAt(Point(5, -1)))
        assertNull(view.indexAt(Point(5, ModelPickerPopup.ROW_HEIGHT * 99)))
    }

    @Test
    fun `clicking a row picks that model`() {
        val picked = mutableListOf<String>()
        val view = ModelListView { picked += it.model }.apply { models = this@ModelListViewTest.models }

        click(view, row = 1)

        assertEquals(listOf("lumen-alpha"), picked)
        assertEquals(1, view.selectedIndex)
    }

    @Test
    fun `each row is two lines tall`() {
        val view = ModelListView {}.apply { models = this@ModelListViewTest.models }

        assertEquals(models.size * ModelPickerPopup.ROW_HEIGHT, view.preferredSize.height)
        assertTrue(view.preferredSize.width >= ModelPickerPopup.MIN_WIDTH)
        assertTrue(view.widthFor() > 0)
    }


    @Test
    fun `the popup shows only two thirds of the rows that fit`() {
        // 600px 可用 → 本来能放 12 行；先取 2/3 再缩 1/3，最终 5 行。
        assertEquals(5, ModelPickerPopup.modelRowsThatFit(600, ModelPickerPopup.ROW_HEIGHT))
        // 高度极小时仍保留最小行数。
        assertEquals(ModelPickerPopup.MIN_ROWS, ModelPickerPopup.modelRowsThatFit(60, ModelPickerPopup.ROW_HEIGHT))
        // 大屏不超过上限。
        assertTrue(
            ModelPickerPopup.modelRowsThatFit(4000, ModelPickerPopup.ROW_HEIGHT) <= ModelPickerPopup.MAX_ROWS,
        )
    }

    private fun click(view: ModelListView, row: Int) {
        val y = row * ModelPickerPopup.ROW_HEIGHT + ModelPickerPopup.ROW_HEIGHT / 2
        listOf(MouseEvent.MOUSE_PRESSED, MouseEvent.MOUSE_RELEASED).forEach { id ->
            view.dispatchEvent(
                MouseEvent(view, id, System.currentTimeMillis(), 0, 1, y, 1, false, MouseEvent.BUTTON1),
            )
        }
    }
}
