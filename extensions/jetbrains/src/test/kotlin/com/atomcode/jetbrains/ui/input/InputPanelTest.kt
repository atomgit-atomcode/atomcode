package com.atomcode.jetbrains.ui.input

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import java.awt.Container
import javax.swing.JButton
import javax.swing.SwingUtilities

class InputPanelTest {
    @Test
    fun `rejected send keeps input text`() {
        SwingUtilities.invokeAndWait {
            val panel = InputPanel(
                onSend = { false },
                onStop = {},
                onAttach = {},
                onSlashCommand = {},
                onClearContext = {},
                onRemoveContext = {},
                onModelSelect = {},
                onPasteFromClipboard = { false },
            )
            panel.setInputText("do not lose this")

            findButton(panel) { it.text.contains("发送") }!!.doClick()

            assertEquals("do not lose this", panel.getInputText())
        }
    }

    private fun findButton(container: Container, predicate: (JButton) -> Boolean): JButton? {
        for (component in container.components) {
            if (component is JButton && predicate(component)) return component
            if (component is Container) {
                val nested = findButton(component, predicate)
                if (nested != null) return nested
            }
        }
        return null
    }
}
