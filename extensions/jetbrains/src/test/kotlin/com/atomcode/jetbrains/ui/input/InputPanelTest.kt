package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ApprovalMode
import com.atomcode.jetbrains.daemon.ModelInfo
import java.awt.Container
import javax.swing.JButton
import javax.swing.SwingUtilities
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

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

    @Test
    fun `a model without effort levels keeps the chip hidden across a layout pass`() {
        SwingUtilities.invokeAndWait {
            val panel = newPanel()
            panel.setEffortChoices(emptyList(), null)

            // The toolbar layout used to re-show every placed component, which
            // resurrected the chip as a lone "Default" for endpoints that declare
            // no levels (the reported bug).
            panel.layoutToolbarForTest(900)

            assertFalse(panel.effortVisibleForTest())
            assertTrue(panel.effortOptionsForTest().isEmpty())
        }
    }

    @Test
    fun `a model with declared levels shows the chip after a layout pass`() {
        SwingUtilities.invokeAndWait {
            val panel = newPanel()
            val model = ModelInfo(
                provider = "AtomGit",
                model = "lumen-alpha",
                providerType = "openai_compat",
                isDefault = true,
                effortApplicable = true,
                effortLevels = listOf("low", "medium", "high"),
                reasoningEffort = "medium",
            )
            panel.setEffortChoices(EffortChoices.forModel(model), model.reasoningEffort)

            panel.layoutToolbarForTest(900)

            assertTrue(panel.effortVisibleForTest())
            assertEquals("Medium ▾", panel.effortDisplayTextForTest())
        }
    }

    private fun newPanel() = InputPanel(
        onSend = { false },
        onStop = {},
        onAttach = {},
        onSlashCommand = {},
        onClearContext = {},
        onRemoveContext = {},
        onModelSelect = {},
        onPasteFromClipboard = { false },
    )

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
