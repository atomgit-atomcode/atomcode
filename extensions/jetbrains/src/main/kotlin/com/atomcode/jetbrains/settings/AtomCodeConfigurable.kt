package com.atomcode.jetbrains.settings

import com.intellij.openapi.options.Configurable
import java.awt.BorderLayout
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JSpinner
import javax.swing.JTextField
import javax.swing.SpinnerNumberModel

class AtomCodeConfigurable : Configurable {
    private var panel: JPanel? = null
    private val settings = AtomCodeSettingsState.getInstance()

    private lateinit var daemonPath: JTextField
    private lateinit var host: JTextField
    private lateinit var port: JSpinner
    private lateinit var autoStart: JCheckBox
    private lateinit var autoSaveBeforeRead: JCheckBox
    private lateinit var timeout: JSpinner
    private lateinit var contextLevel: JComboBox<AtomCodeContextLevel>
    private lateinit var allowSelection: JCheckBox
    private lateinit var sendRelativePath: JCheckBox
    private lateinit var sendWithCtrlEnter: JCheckBox
    private lateinit var chatFontSize: JSpinner

    override fun getDisplayName(): String = "AtomCode"

    override fun createComponent(): JComponent {
        val form = JPanel(GridBagLayout())
        var row = 0

        daemonPath = JTextField()
        host = JTextField()
        port = JSpinner(SpinnerNumberModel(13456, 1, 65535, 1))
        autoStart = JCheckBox("Auto-start daemon after user action")
        autoSaveBeforeRead = JCheckBox("Auto-save files before AtomCode reads them")
        timeout = JSpinner(SpinnerNumberModel(30_000, 1_000, 300_000, 1_000))
        contextLevel = JComboBox(AtomCodeContextLevel.entries.toTypedArray())
        allowSelection = JCheckBox("Allow selected text context")
        sendRelativePath = JCheckBox("Send relative path with selection")
        sendWithCtrlEnter = JCheckBox("Use Ctrl+Enter to send chat messages")
        chatFontSize = JSpinner(SpinnerNumberModel(13, 9, 30, 1))

        form.addRow(row++, "Daemon binary path", daemonPath)
        form.addRow(row++, "Host", host)
        form.addRow(row++, "Port", port)
        form.addRow(row++, "Request timeout (ms)", timeout)
        form.addRow(row++, "Chat font size", chatFontSize)
        form.addRow(row++, "Context level", contextLevel)
        form.addFullRow(row++, autoStart)
        form.addFullRow(row++, autoSaveBeforeRead)
        form.addFullRow(row++, allowSelection)
        form.addFullRow(row++, sendRelativePath)
        form.addFullRow(row++, sendWithCtrlEnter)

        panel = JPanel(BorderLayout()).apply {
            add(form, BorderLayout.NORTH)
        }
        reset()
        return panel!!
    }

    override fun isModified(): Boolean {
        val current = settings.state
        return daemonPath.text != current.daemonBinaryPath ||
            host.text != current.host ||
            port.value as Int != current.port ||
            autoStart.isSelected != current.autoStart ||
            autoSaveBeforeRead.isSelected != current.autoSaveBeforeRead ||
            timeout.value as Int != current.requestTimeoutMs ||
            chatFontSize.value as Int != current.chatFontSize ||
            contextLevel.selectedItem != current.contextLevel ||
            allowSelection.isSelected != current.allowSelectedTextContext ||
            sendRelativePath.isSelected != current.sendRelativePathWithSelection ||
            sendWithCtrlEnter.isSelected != current.sendWithCtrlEnter
    }

    override fun apply() {
        settings.update {
            it.daemonBinaryPath = daemonPath.text.trim()
            it.host = host.text.trim()
            it.port = port.value as Int
            it.autoStart = autoStart.isSelected
            it.autoSaveBeforeRead = autoSaveBeforeRead.isSelected
            it.requestTimeoutMs = timeout.value as Int
            it.chatFontSize = chatFontSize.value as Int
            it.contextLevel = contextLevel.selectedItem as AtomCodeContextLevel
            it.allowSelectedTextContext = allowSelection.isSelected
            it.sendRelativePathWithSelection = sendRelativePath.isSelected
            it.sendWithCtrlEnter = sendWithCtrlEnter.isSelected
        }
    }

    override fun reset() {
        val current = settings.state
        daemonPath.text = current.daemonBinaryPath
        host.text = current.host
        port.value = current.port
        autoStart.isSelected = current.autoStart
        autoSaveBeforeRead.isSelected = current.autoSaveBeforeRead
        timeout.value = current.requestTimeoutMs
        chatFontSize.value = current.chatFontSize
        contextLevel.selectedItem = current.contextLevel
        allowSelection.isSelected = current.allowSelectedTextContext
        sendRelativePath.isSelected = current.sendRelativePathWithSelection
        sendWithCtrlEnter.isSelected = current.sendWithCtrlEnter
    }

    private fun JPanel.addRow(row: Int, label: String, component: JComponent) {
        add(JLabel(label), GridBagConstraints().apply {
            gridx = 0
            gridy = row
            anchor = GridBagConstraints.WEST
            insets.set(4, 4, 4, 8)
        })
        add(component, GridBagConstraints().apply {
            gridx = 1
            gridy = row
            weightx = 1.0
            fill = GridBagConstraints.HORIZONTAL
            insets.set(4, 0, 4, 4)
        })
    }

    private fun JPanel.addFullRow(row: Int, component: JComponent) {
        add(component, GridBagConstraints().apply {
            gridx = 0
            gridy = row
            gridwidth = 2
            anchor = GridBagConstraints.WEST
            insets.set(4, 4, 4, 4)
        })
    }
}
