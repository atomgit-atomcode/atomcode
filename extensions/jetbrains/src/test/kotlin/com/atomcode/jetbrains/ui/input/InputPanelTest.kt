package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ApprovalMode
import java.awt.Container
import javax.swing.JButton
import javax.swing.SwingUtilities
import kotlin.test.Test
import kotlin.test.assertEquals

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

    @Test
    fun `approval mode selector exposes all modes and dispatches selection`() {
        val selected = mutableListOf<ApprovalMode>()
        val panel = InputPanel(
            onSend = { false },
            onStop = {},
            onAttach = {},
            onSlashCommand = {},
            onClearContext = {},
            onRemoveContext = {},
            onModelSelect = {},
            onApprovalModeSelect = { selected += it },
            onPasteFromClipboard = { false },
        )

        assertEquals(
            listOf("Build", "Accept Edits", "Auto", "Plan"),
            panel.approvalModeOptionsForTest(),
        )

        panel.setApprovalMode(ApprovalMode.AcceptEdits)
        assertEquals("Accept Edits ▾", panel.approvalModeDisplayTextForTest())

        panel.selectApprovalModeForTest(ApprovalMode.Auto)
        assertEquals(listOf(ApprovalMode.Auto), selected)
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
