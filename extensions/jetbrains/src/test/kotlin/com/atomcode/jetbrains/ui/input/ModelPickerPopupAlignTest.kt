package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo
import java.awt.BorderLayout
import java.awt.GraphicsEnvironment
import javax.swing.JFrame
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.SwingUtilities
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * 模型弹窗要和控件（圆角边框）**右沿对齐**：截图里它是往左悬出去的。
 */
class ModelPickerPopupAlignTest {
    private fun onEdt(block: () -> Unit) {
        val t = Thread { SwingUtilities.invokeAndWait(block) }
        t.start()
        t.join()
    }

    private fun model(provider: String, name: String, isDefault: Boolean = false) = ModelInfo(
        provider = provider,
        model = name,
        providerType = "openai_compat",
        isDefault = isDefault,
    )

    @Test
    fun `the popup right edge lines up with the control`() {
        if (GraphicsEnvironment.isHeadless()) return
        val models = (1..8).map { model("AtomGit-model-$it", "model-$it", isDefault = it == 3) }
        onEdt {
            val frame = JFrame("model-popup-align")
            val chip = JLabel("lumen-alpha ▾").apply { border = RoundedLineBorder() }
            val bar = JPanel(BorderLayout()).apply { add(chip, BorderLayout.EAST) }
            frame.contentPane.add(JPanel(BorderLayout()).apply { add(bar, BorderLayout.SOUTH) })
            frame.setSize(900, 700)
            frame.isVisible = true
            try {
                val popup = ModelPickerPopup(anchor = { chip })
                popup.show(models)

                val location = popup.popupLocationOnScreen()
                assertTrue(location != null, "弹窗应当已显示")
                val chipRight = chip.locationOnScreen.x + chip.width
                val popupRight = location!!.x + popup.popupBounds().width
                assertEquals(chipRight, popupRight, "弹窗右沿应与控件右沿对齐")

                popup.hide()
            } finally {
                frame.dispose()
            }
        }
    }
}
