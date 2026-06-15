package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.ChatStreamListener
import com.atomcode.jetbrains.daemon.ConnectionState
import com.atomcode.jetbrains.daemon.CreateProviderRequest
import com.atomcode.jetbrains.daemon.MessageInfo
import com.atomcode.jetbrains.daemon.ModelInfo
import com.atomcode.jetbrains.daemon.PatchProviderRequest
import com.atomcode.jetbrains.daemon.PatchThinkingRequest
import com.atomcode.jetbrains.daemon.ProviderInfo
import com.atomcode.jetbrains.daemon.SessionDetail
import com.atomcode.jetbrains.daemon.SessionMeta
import com.atomcode.jetbrains.daemon.SetupSnapshot
import com.atomcode.jetbrains.diagnostics.AtomCodeDiagnostics
import com.atomcode.jetbrains.actions.openAtomCodeSettings
import com.atomcode.jetbrains.security.PathSensitivity
import com.atomcode.jetbrains.security.SensitivePathClassifier
import com.atomcode.jetbrains.services.AtomCodeProjectService
import com.atomcode.jetbrains.services.SessionRefView
import com.atomcode.jetbrains.settings.AtomCodeContextLevel
import com.atomcode.jetbrains.settings.AtomCodeSettingsState
import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.openapi.fileChooser.FileChooser
import com.intellij.openapi.fileChooser.FileChooserDescriptor
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.openapi.command.WriteCommandAction
import com.intellij.openapi.Disposable
import com.intellij.openapi.ide.CopyPasteManager
import java.awt.BorderLayout
import java.awt.Dialog
import java.awt.Dimension
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Insets
import java.awt.datatransfer.StringSelection
import java.beans.PropertyChangeEvent
import javax.swing.DefaultListModel
import javax.swing.AbstractAction
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JDialog
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JMenu
import javax.swing.JMenuItem
import javax.swing.JPanel
import javax.swing.JPasswordField
import javax.swing.JPopupMenu
import javax.swing.KeyStroke
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.JSeparator
import javax.swing.BorderFactory
import javax.swing.ListSelectionModel
import javax.swing.JOptionPane
import javax.swing.SwingUtilities
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

private const val MAX_ATTACHED_FILE_CHARS = 120_000

class AtomCodeChatPanel(private val project: Project) : JPanel(BorderLayout()), Disposable {
    private val service = AtomCodeProjectService.getInstance(project)
    private val settings = AtomCodeSettingsState.getInstance()
    private val status = JLabel("Idle").apply {
        toolTipText = "Click to connect/reconnect"
        cursor = java.awt.Cursor.getPredefinedCursor(java.awt.Cursor.HAND_CURSOR)
        addMouseListener(object : java.awt.event.MouseAdapter() {
            override fun mouseClicked(e: java.awt.event.MouseEvent) { connect() }
        })
    }
    private val contextChips = JPanel(java.awt.FlowLayout(java.awt.FlowLayout.LEFT, 4, 2))
    private val messages = JTextArea().apply {
        isEditable = false
        lineWrap = true
        wrapStyleWord = true
    }
    private val input = JTextArea(4, 20).apply {
        lineWrap = true
        wrapStyleWord = true
    }
    private val send = JButton("Send").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
    }
    private val attachFile = JButton("📎").apply {
        toolTipText = "Attach files"
        isContentAreaFilled = false
        isBorderPainted = false
    }
    private val stop = JButton("⏹").apply {
        isEnabled = false
        toolTipText = "Stop generation"
        isContentAreaFilled = false
        isBorderPainted = false
    }
    private val clearContext = JButton("Clear all").apply { isEnabled = false }
    private val modelPicker = JComboBox<ModelInfo>().apply {
        prototypeDisplayValue = ModelInfo("provider", "model-name", "openai", false)
    }
    private val sessionPicker = JComboBox<SessionMeta>().apply {
        prototypeDisplayValue = SessionMeta("00000000", "Recent conversation title", "", 0L, 99)
    }

    private var loadingSessions = false
    private var loadingModels = false
    private var generating = false
    private var setupSnapshot: SetupSnapshot? = null
    private var currentSession: SessionRefView? = null
    private val currentAssistantText = StringBuilder()
    private var lastAssistantText = ""
    private val pendingContext = mutableListOf<ChatContextItem>()
    private val queuedPrompts = ArrayDeque<QueuedPrompt>()
    private var disposed = false
    private val connectionListener = java.beans.PropertyChangeListener { event: PropertyChangeEvent ->
        if (disposed) return@PropertyChangeListener
        SwingUtilities.invokeLater {
            if (!disposed) {
                renderConnectionState(event.newValue as ConnectionState)
            }
        }
    }

    init {
        minimumSize = Dimension(280, 300)

        // ── Header: compact toolbar ──
        val header = JPanel(java.awt.FlowLayout(java.awt.FlowLayout.LEFT, 6, 2)).apply {
            border = javax.swing.BorderFactory.createEmptyBorder(2, 4, 2, 4)
        }
        header.add(status)
        header.add(JLabel("Model:"))
        header.add(modelPicker)
        header.add(JLabel("Session:"))
        header.add(sessionPicker)

        // ── Footer: context chips + bordered input + compact button bar ──
        val inputScroll = JScrollPane(input).apply {
            border = BorderFactory.createCompoundBorder(
                BorderFactory.createLineBorder(java.awt.Color(0xBB, 0xBB, 0xBB), 1),
                BorderFactory.createEmptyBorder(4, 6, 4, 6),
            )
            preferredSize = Dimension(280, 90)
            minimumSize = Dimension(200, 60)
        }

        val buttonBar = JPanel(java.awt.BorderLayout(0, 0)).apply {
            border = BorderFactory.createEmptyBorder(2, 0, 0, 0)
            val leftButtons = JPanel(java.awt.FlowLayout(java.awt.FlowLayout.LEFT, 2, 0)).apply {
                add(attachFile)
            }
            val rightButtons = JPanel(java.awt.FlowLayout(java.awt.FlowLayout.RIGHT, 4, 0)).apply {
                add(stop)
                add(send)
            }
            add(leftButtons, java.awt.BorderLayout.WEST)
            add(rightButtons, java.awt.BorderLayout.EAST)
        }

        val footer = JPanel(BorderLayout(0, 2)).apply {
            border = BorderFactory.createEmptyBorder(4, 6, 6, 6)
        }
        contextChips.isVisible = false
        footer.add(contextChips, BorderLayout.NORTH)
        footer.add(inputScroll, BorderLayout.CENTER)
        footer.add(buttonBar, BorderLayout.SOUTH)

        add(header, BorderLayout.NORTH)
        add(JScrollPane(messages), BorderLayout.CENTER)
        add(footer, BorderLayout.SOUTH)

        // ── Action bindings ──
        clearContext.addActionListener { clearPendingContext() }
        attachFile.addActionListener { chooseFilesForContext() }
        modelPicker.addActionListener {
            if (!loadingModels) {
                (modelPicker.selectedItem as? ModelInfo)?.let(::setDefaultModel)
            }
        }
        sessionPicker.addActionListener {
            if (!loadingSessions) {
                (sessionPicker.selectedItem as? SessionMeta)?.let(::loadSession)
            }
        }
        send.addActionListener { sendPrompt() }
        installInputKeyBindings()
        stop.addActionListener { stopCurrentGeneration() }

        service.addConnectionListener(connectionListener)
        renderConnectionState(service.connectionState)
        applyChatSettings()
    }

    override fun dispose() {
        if (disposed) return
        disposed = true
        queuedPrompts.clear()
        pendingContext.clear()
        service.removeConnectionListener(connectionListener)
    }

    fun focusInput() {
        input.requestFocusInWindow()
    }

    fun submitPrompt(prompt: String) {
        input.text = prompt
        sendPrompt()
    }

    fun stopCurrentGeneration() {
        queuedPrompts.clear()
        renderQueueState()
        if (!generating) {
            service.stopGeneration(currentSession?.id)
            return
        }
        append("\n[Stopping]\n")
        service.stopGeneration(currentSession?.id).whenComplete { _, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode stop failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                }
            }
        }
        finishPrompt()
    }

    fun addContext(item: ChatContextItem) {
        val duplicate = pendingContext.any {
            it.path == item.path && it.startLine == item.startLine && it.endLine == item.endLine && it.selection == item.selection
        }
        if (!duplicate) {
            pendingContext += item
        }
        rebuildContextChips()
        focusInput()
    }

    private fun connect() {
        status.text = "Checking daemon..."
        service.ensureConnected().thenRun {
            SwingUtilities.invokeLater {
                refreshSetupSnapshot()
                refreshSessionList()
            }
        }
    }

    private fun refreshSetupSnapshot() {
        service.loadSetupSnapshot().whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to load setup"}\n")
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
            }
        }
    }

    private fun renderSetupSnapshot(snapshot: SetupSnapshot) {
        setupSnapshot = snapshot

        loadingModels = true
        modelPicker.removeAllItems()
        snapshot.models.forEach(modelPicker::addItem)
        snapshot.models.firstOrNull { it.isDefault }?.let {
            modelPicker.selectedItem = it
        }
        modelPicker.isEnabled = snapshot.models.isNotEmpty()
        loadingModels = false
    }

    private fun login() {
        service.loginWithBrowser { message ->
            SwingUtilities.invokeLater {
                status.text = "Login: $message"
            }
        }.whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode login failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                append("AtomCode: Login complete.\n")
            }
        }
    }

    private fun setDefaultModel(model: ModelInfo) {
        modelPicker.isEnabled = false
        service.setDefaultModel(model).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                modelPicker.isEnabled = true
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to set default model"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                append("AtomCode: Default model set to ${model.model}.\n")
            }
        }
    }

    private fun runSetup() {
        status.text = "Setup: running..."
        service.setupCodingPlan().whenComplete { report, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode setup failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                append("AtomCode setup:\n$report\n")
                refreshSetupSnapshot()
            }
        }
    }

    private fun showCreateProviderDialog() {
        val name = JTextField("default")
        val type = JComboBox(arrayOf("openai", "claude", "ollama"))
        val model = JTextField("gpt-4o-mini")
        val apiKey = JPasswordField()
        val baseUrl = JTextField()
        val setDefault = JCheckBox("Set as default", true)

        val form = JPanel(GridBagLayout())
        fun addRow(row: Int, label: String, field: java.awt.Component) {
            form.add(
                JLabel(label),
                GridBagConstraints().apply {
                    gridx = 0
                    gridy = row
                    anchor = GridBagConstraints.WEST
                    insets = Insets(4, 4, 4, 8)
                },
            )
            form.add(
                field,
                GridBagConstraints().apply {
                    gridx = 1
                    gridy = row
                    weightx = 1.0
                    fill = GridBagConstraints.HORIZONTAL
                    insets = Insets(4, 4, 4, 4)
                },
            )
        }

        addRow(0, "Name", name)
        addRow(1, "Type", type)
        addRow(2, "Model", model)
        addRow(3, "API Key", apiKey)
        addRow(4, "Base URL", baseUrl)
        form.add(
            setDefault,
            GridBagConstraints().apply {
                gridx = 1
                gridy = 5
                anchor = GridBagConstraints.WEST
                insets = Insets(4, 4, 4, 4)
            },
        )

        val choice = JOptionPane.showConfirmDialog(
            this,
            form,
            "Create AtomCode Provider",
            JOptionPane.OK_CANCEL_OPTION,
            JOptionPane.PLAIN_MESSAGE,
        )
        if (choice != JOptionPane.OK_OPTION) return

        val request = CreateProviderRequest(
            name = name.text.trim(),
            type = (type.selectedItem as? String).orEmpty(),
            model = model.text.trim(),
            apiKey = String(apiKey.password).trim().ifBlank { null },
            baseUrl = baseUrl.text.trim().ifBlank { null },
            setDefault = setDefault.isSelected,
        )
        if (request.name.isBlank() || request.type.isBlank() || request.model.isBlank()) {
            Messages.showWarningDialog(this, "Name, type, and model are required.", "AtomCode")
            return
        }

        service.createProvider(request).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode provider failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                append("AtomCode: Provider ${request.name} saved.\n")
            }
        }
    }

    private fun showEditProviderDialog() {
        val selected = selectedProvider() ?: return
        val name = JTextField(selected.name)
        val type = JComboBox(arrayOf("openai", "claude", "ollama")).apply {
            selectedItem = selected.type.ifBlank { "openai" }
        }
        val model = JTextField(selected.model)
        val apiKey = JPasswordField()
        val clearApiKey = JCheckBox("Clear API key", false)
        val baseUrl = JTextField()
        val clearBaseUrl = JCheckBox("Clear Base URL", false)

        val form = JPanel(GridBagLayout())
        fun addRow(row: Int, label: String, field: java.awt.Component) {
            form.add(
                JLabel(label),
                GridBagConstraints().apply {
                    gridx = 0
                    gridy = row
                    anchor = GridBagConstraints.WEST
                    insets = Insets(4, 4, 4, 8)
                },
            )
            form.add(
                field,
                GridBagConstraints().apply {
                    gridx = 1
                    gridy = row
                    weightx = 1.0
                    fill = GridBagConstraints.HORIZONTAL
                    insets = Insets(4, 4, 4, 4)
                },
            )
        }

        addRow(0, "Name", name)
        addRow(1, "Type", type)
        addRow(2, "Model", model)
        addRow(3, "New API Key", apiKey)
        addRow(4, "Base URL", baseUrl)
        form.add(
            clearApiKey,
            GridBagConstraints().apply {
                gridx = 1
                gridy = 5
                anchor = GridBagConstraints.WEST
                insets = Insets(4, 4, 4, 4)
            },
        )
        form.add(
            clearBaseUrl,
            GridBagConstraints().apply {
                gridx = 1
                gridy = 6
                anchor = GridBagConstraints.WEST
                insets = Insets(4, 4, 4, 4)
            },
        )

        val choice = JOptionPane.showConfirmDialog(
            this,
            form,
            "Edit AtomCode Provider",
            JOptionPane.OK_CANCEL_OPTION,
            JOptionPane.PLAIN_MESSAGE,
        )
        if (choice != JOptionPane.OK_OPTION) return

        val request = PatchProviderRequest(
            originalName = selected.name,
            name = name.text.trim(),
            type = (type.selectedItem as? String).orEmpty(),
            model = model.text.trim(),
            apiKey = String(apiKey.password).trim().ifBlank { null },
            clearApiKey = clearApiKey.isSelected,
            baseUrl = baseUrl.text.trim().ifBlank { null },
            clearBaseUrl = clearBaseUrl.isSelected,
        )
        if (request.name.isBlank() || request.type.isBlank() || request.model.isBlank()) {
            Messages.showWarningDialog(this, "Name, type, and model are required.", "AtomCode")
            return
        }

        service.patchProvider(request).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode provider update failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                append("AtomCode: Provider ${request.name} updated.\n")
            }
        }
    }

    private fun deleteSelectedProvider() {
        val selected = selectedProvider() ?: return
        val choice = Messages.showYesNoDialog(
            this,
            "Delete provider \"${selected.name}\" from AtomCode config?",
            "AtomCode",
            Messages.getWarningIcon(),
        )
        if (choice != Messages.YES) return

        service.deleteProvider(selected.name).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode provider delete failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                append("AtomCode: Provider ${selected.name} deleted.\n")
            }
        }
    }

    private fun showThinkingDialog() {
        val selected = selectedProvider() ?: return
        val enabled = JCheckBox("Enable thinking/reasoning", selected.thinkingEnabled)
        val budget = JTextField(selected.thinkingBudget?.toString() ?: "10000")
        val type = JTextField(selected.thinkingType.orEmpty())
        val keep = JTextField(selected.thinkingKeep.orEmpty())

        val form = JPanel(GridBagLayout())
        fun addRow(row: Int, label: String, field: java.awt.Component) {
            form.add(
                JLabel(label),
                GridBagConstraints().apply {
                    gridx = 0
                    gridy = row
                    anchor = GridBagConstraints.WEST
                    insets = Insets(4, 4, 4, 8)
                },
            )
            form.add(
                field,
                GridBagConstraints().apply {
                    gridx = 1
                    gridy = row
                    weightx = 1.0
                    fill = GridBagConstraints.HORIZONTAL
                    insets = Insets(4, 4, 4, 4)
                },
            )
        }

        form.add(
            enabled,
            GridBagConstraints().apply {
                gridx = 1
                gridy = 0
                anchor = GridBagConstraints.WEST
                insets = Insets(4, 4, 4, 4)
            },
        )
        addRow(1, "Budget", budget)
        addRow(2, "Type", type)
        addRow(3, "Keep", keep)

        val choice = JOptionPane.showConfirmDialog(
            this,
            form,
            "AtomCode Thinking - ${selected.name}",
            JOptionPane.OK_CANCEL_OPTION,
            JOptionPane.PLAIN_MESSAGE,
        )
        if (choice != JOptionPane.OK_OPTION) return

        val budgetValue = budget.text.trim().takeIf { it.isNotBlank() }?.toIntOrNull()
        if (budget.text.trim().isNotBlank() && budgetValue == null) {
            Messages.showWarningDialog(this, "Thinking budget must be a number.", "AtomCode")
            return
        }

        service.patchProviderThinking(
            selected.name,
            PatchThinkingRequest(
                enabled = enabled.isSelected,
                budget = budgetValue,
                type = type.text.trim().ifBlank { null },
                keep = keep.text.trim().ifBlank { null },
            ),
        ).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode thinking update failed: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                val state = if (enabled.isSelected) "enabled" else "disabled"
                append("AtomCode: Thinking $state for ${selected.name}.\n")
            }
        }
    }

    private fun selectedProvider(): ProviderInfo? {
        val selectedModel = modelPicker.selectedItem as? ModelInfo
        val snapshot = setupSnapshot ?: return null
        return selectedModel?.let { model ->
            snapshot.providers.firstOrNull { it.name == model.provider }
        } ?: snapshot.providers.firstOrNull { it.isDefault } ?: snapshot.providers.firstOrNull()
    }

    fun startNewConversation() {
        service.createSession().whenComplete { session, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to create session"}\n")
                    return@invokeLater
                }
                currentSession = session
                messages.text = ""
                append("AtomCode: Started new session ${session.name.ifBlank { session.id.take(8) }}.\n")
                refreshSessionList()
                input.requestFocusInWindow()
            }
        }
    }

    private fun showSessionHistory() {
        service.refreshSessions().whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to load sessions"}\n")
                    return@invokeLater
                }
                openSessionHistoryDialog(sessions)
            }
        }
    }

    private fun openSessionHistoryDialog(initialSessions: List<SessionMeta>) {
        var sessions = initialSessions.sortedByDescending { it.updatedAt }
        val model = DefaultListModel<SessionMeta>()
        val search = JTextField()
        val list = JList(model).apply {
            selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
            visibleRowCount = 14
        }
        val load = JButton("Load")
        val rename = JButton("Rename")
        val delete = JButton("Delete Selected")
        val refresh = JButton("Refresh")
        val close = JButton("Close")

        fun updateHistoryButtons() {
            val selectedCount = list.selectedValuesList.size
            load.isEnabled = selectedCount == 1
            rename.isEnabled = selectedCount == 1
            delete.isEnabled = selectedCount > 0
        }

        fun refill() {
            val query = search.text.trim().lowercase()
            model.clear()
            sessions
                .filter {
                    query.isBlank() ||
                        it.displayName.lowercase().contains(query) ||
                        it.id.lowercase().contains(query)
                }
                .forEach(model::addElement)
            val hasItems = model.size() > 0
            if (hasItems && list.selectedIndex < 0) {
                list.selectedIndex = 0
            }
            updateHistoryButtons()
        }

        list.addListSelectionListener {
            if (!it.valueIsAdjusting) {
                updateHistoryButtons()
            }
        }

        search.document.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent) = refill()
            override fun removeUpdate(e: DocumentEvent) = refill()
            override fun changedUpdate(e: DocumentEvent) = refill()
        })

        val panel = JPanel(BorderLayout(8, 8)).apply {
            add(search, BorderLayout.NORTH)
            add(JScrollPane(list), BorderLayout.CENTER)
            add(JPanel().apply {
                add(load)
                add(rename)
                add(delete)
                add(refresh)
                add(close)
            }, BorderLayout.SOUTH)
            preferredSize = Dimension(560, 360)
        }
        val dialog = JDialog(SwingUtilities.getWindowAncestor(this), "AtomCode Session History", Dialog.ModalityType.APPLICATION_MODAL).apply {
            contentPane = panel
            pack()
            setLocationRelativeTo(this@AtomCodeChatPanel)
        }

        load.addActionListener {
            val selected = list.selectedValue ?: return@addActionListener
            dialog.dispose()
            loadSession(selected)
        }
        rename.addActionListener {
            val selected = list.selectedValue ?: return@addActionListener
            val nextName = JOptionPane.showInputDialog(dialog, "Session name", selected.displayName)?.trim() ?: return@addActionListener
            if (nextName.isBlank()) {
                Messages.showWarningDialog(dialog, "Session name cannot be empty.", "AtomCode")
                return@addActionListener
            }
            rename.isEnabled = false
            service.renameSession(selected, nextName).whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    rename.isEnabled = true
                    if (error != null) {
                        append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to rename session"}\n")
                        return@invokeLater
                    }
                    sessions = updated.sortedByDescending { it.updatedAt }
                    replaceSessions(sessions, selected.id)
                    refill()
                    append("AtomCode: Session renamed to $nextName.\n")
                }
            }
        }
        delete.addActionListener {
            val selected = list.selectedValuesList
            if (selected.isEmpty()) return@addActionListener
            val label = if (selected.size == 1) {
                "Delete AtomCode session \"${selected.first().displayName}\" from local history?"
            } else {
                "Delete ${selected.size} AtomCode sessions from local history?"
            }
            val choice = Messages.showYesNoDialog(
                dialog,
                label,
                "AtomCode",
                Messages.getWarningIcon(),
            )
            if (choice != Messages.YES) return@addActionListener
            delete.isEnabled = false
            service.deleteSessions(selected).whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    delete.isEnabled = true
                    if (error != null) {
                        append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to delete sessions"}\n")
                        return@invokeLater
                    }
                    sessions = updated.sortedByDescending { it.updatedAt }
                    if (selected.any { it.id == currentSession?.id }) {
                        currentSession = null
                    }
                    replaceSessions(sessions, currentSession?.id)
                    if (currentSession == null) {
                        messages.text = ""
                    }
                    refill()
                    append("AtomCode: Deleted ${selected.size} session(s).\n")
                }
            }
        }
        refresh.addActionListener {
            refresh.isEnabled = false
            service.refreshSessions().whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    refresh.isEnabled = true
                    if (error != null) {
                        append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to refresh sessions"}\n")
                        return@invokeLater
                    }
                    sessions = updated.sortedByDescending { it.updatedAt }
                    replaceSessions(sessions, currentSession?.id)
                    refill()
                }
            }
        }
        close.addActionListener { dialog.dispose() }

        refill()
        dialog.isVisible = true
    }

    private fun renameSelectedSession() {
        val selected = sessionPicker.selectedItem as? SessionMeta ?: return
        val nextName = JOptionPane.showInputDialog(
            this,
            "Session name",
            selected.displayName,
        )?.trim() ?: return
        if (nextName.isBlank()) {
            Messages.showWarningDialog(this, "Session name cannot be empty.", "AtomCode")
            return
        }

        service.renameSession(selected, nextName).whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to rename session"}\n")
                    return@invokeLater
                }
                replaceSessions(sessions, selected.id)
                append("AtomCode: Session renamed to $nextName.\n")
            }
        }
    }

    private fun deleteSelectedSession() {
        val selected = sessionPicker.selectedItem as? SessionMeta ?: return
        val choice = Messages.showYesNoDialog(
            this,
            "Delete AtomCode session \"${selected.displayName}\" from local history?",
            "AtomCode",
            Messages.getWarningIcon(),
        )
        if (choice != Messages.YES) return

        service.deleteSession(selected).whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to delete session"}\n")
                    return@invokeLater
                }
                if (currentSession?.id == selected.id) {
                    currentSession = null
                }
                replaceSessions(sessions, currentSession?.id)
                if (currentSession == null) {
                    messages.text = ""
                }
                append("AtomCode: Session deleted.\n")
            }
        }
    }

    fun openProjectChanges() {
        service.fileChangeService.openChangedFiles().whenComplete { files, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to open changes"}\n")
                    service.fileChangeService.openLocalChanges()
                    return@invokeLater
                }
                if (files.isEmpty()) {
                    append("AtomCode: No Git changes found. Opened Local Changes.\n")
                    service.fileChangeService.openLocalChanges()
                } else {
                    append("AtomCode: Opened changed files: ${files.joinToString()}\n")
                }
            }
        }
    }

    private fun showDiagnostics() {
        val snapshot = setupSnapshot
        val state = settings.state
        val details = buildString {
            appendLine("Connection: ${service.connectionState}")
            appendLine("Active session: ${currentSession?.id ?: "(none)"}")
            appendLine("Daemon host: ${state.host}")
            appendLine("Daemon port: ${state.port}")
            appendLine("Daemon binary path: ${state.daemonBinaryPath.ifBlank { "(auto-detect)" }}")
            appendLine("Auto-start: ${state.autoStart}")
            appendLine("Auto-save before read: ${state.autoSaveBeforeRead}")
            appendLine("Context level: ${state.contextLevel}")
            appendLine("Allow selected text context: ${state.allowSelectedTextContext}")
            appendLine("Send relative path with selection: ${state.sendRelativePathWithSelection}")
            appendLine("Send with Ctrl+Enter: ${state.sendWithCtrlEnter}")
            appendLine("Chat font size: ${state.chatFontSize}")
            appendLine("Pending context items: ${pendingContext.size}")
            appendLine("Queued prompts: ${queuedPrompts.size}")
            if (snapshot != null) {
                appendLine("Setup required: ${snapshot.setupRequired}")
                appendLine("Signed in: ${snapshot.auth?.loggedIn ?: false}")
                appendLine("User: ${snapshot.auth?.userName ?: "(none)"}")
                appendLine("Providers: ${snapshot.providers.size}")
                appendLine("Default provider: ${snapshot.defaultProvider.ifBlank { "(none)" }}")
                appendLine("Current model: ${snapshot.currentModel.ifBlank { "(none)" }}")
            } else {
                appendLine("Setup snapshot: not loaded")
            }
        }
        val text = AtomCodeDiagnostics.summary(project, details)
        CopyPasteManager.getInstance().setContents(StringSelection(text))

        val area = JTextArea(text).apply {
            isEditable = false
            lineWrap = false
            rows = 22
            columns = 72
        }
        JOptionPane.showMessageDialog(
            this,
            JScrollPane(area),
            "AtomCode Diagnostics (copied)",
            JOptionPane.INFORMATION_MESSAGE,
        )
    }

    private fun refreshSessionList() {
        service.refreshSessions().whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to load sessions"}\n")
                    return@invokeLater
                }
                replaceSessions(sessions, currentSession?.id)
            }
        }
    }

    private fun replaceSessions(sessions: List<SessionMeta>, selectedSessionId: String?) {
        loadingSessions = true
        sessionPicker.removeAllItems()
        sessions.forEach(sessionPicker::addItem)
        selectedSessionId?.let { active ->
            val match = (0 until sessionPicker.itemCount)
                .map { sessionPicker.getItemAt(it) }
                .firstOrNull { it.id == active }
            if (match != null) {
                sessionPicker.selectedItem = match
            }
        }
        val hasSessions = sessionPicker.itemCount > 0
        loadingSessions = false
    }

    private fun replaceSelectedSession(sessionId: String?) {
        if (sessionId == null) return
        loadingSessions = true
        val match = (0 until sessionPicker.itemCount)
            .map { sessionPicker.getItemAt(it) }
            .firstOrNull { it.id == sessionId }
        if (match != null) {
            sessionPicker.selectedItem = match
        }
        loadingSessions = false
    }

    private fun loadSession(meta: SessionMeta) {
        service.loadSessionDetail(meta).whenComplete { detail, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed to load session"}\n")
                    return@invokeLater
                }
                currentSession = SessionRefView(detail.id, detail.name, detail.projectHash, detail.workingDir)
                replaceSelectedSession(detail.id)
                renderSession(detail)
                input.requestFocusInWindow()
            }
        }
    }

    private fun renderSession(detail: SessionDetail) {
        messages.text = ""
        append("AtomCode: Loaded ${detail.name.ifBlank { detail.id.take(8) }}.\n\n")
        detail.messages.forEach(::renderHistoryMessage)
    }

    private fun renderHistoryMessage(message: MessageInfo) {
        val label = when (message.role) {
            "user" -> "You"
            "assistant" -> "AtomCode"
            "tool" -> "Tool"
            "system" -> "System"
            else -> message.role.ifBlank { "Message" }
        }
        append("$label: ${message.content}\n\n")
    }

    private fun sendPrompt() {
        val prompt = input.text.trim()
        if (prompt.isEmpty()) return
        if (handleLocalInputCommand(prompt)) {
            input.text = ""
            return
        }
        val transformedPrompt = slashPromptTemplate(prompt) ?: prompt
        val pendingContextForSend = pendingContext.toList()
        val contextForSend = pendingContextForSend + buildAutomaticContext(pendingContextForSend)
        val message = buildPromptWithContext(transformedPrompt, contextForSend)
        val contextNames = contextForSend.map { it.displayName }
        if (generating) {
            queuedPrompts += QueuedPrompt(transformedPrompt, message, contextNames)
            append("[Queued] $transformedPrompt\n")
            if (contextNames.isNotEmpty()) {
                append("[Queued context] ${contextNames.joinToString()}\n")
            }
            if (pendingContextForSend.isNotEmpty()) {
                clearPendingContext()
            }
            input.text = ""
            renderQueueState()
            return
        }
        if (pendingContextForSend.isNotEmpty()) {
            clearPendingContext()
        }
        startPrompt(transformedPrompt, message, contextNames)
    }

    private fun startPrompt(prompt: String, message: String, contextNames: List<String>) {
        append("You: $prompt\n")
        if (contextNames.isNotEmpty()) {
            append("[Context] ${contextNames.joinToString()}\n")
        }
        input.text = ""
        generating = true
        currentAssistantText.setLength(0)
        send.isEnabled = true
        send.text = "Queue"
        stop.isEnabled = true
        var assistantStarted = false
        service.sendPrompt(message, currentSession, object : ChatStreamListener {
            override fun onEvent(event: ChatEvent) {
                SwingUtilities.invokeLater {
                    assistantStarted = renderChatEvent(event, assistantStarted)
                    if (isTerminalEvent(event)) {
                        finishPromptAndContinue()
                    }
                }
            }

            override fun onComplete() {
                SwingUtilities.invokeLater {
                    if (!assistantStarted) {
                        append("AtomCode: completed without streamed output.\n")
                    } else {
                        append("\n")
                    }
                    finishPromptAndContinue()
                }
            }

            override fun onError(message: String) {
                SwingUtilities.invokeLater {
                    append("AtomCode: $message\n")
                    finishPromptAndContinue()
                }
            }
        }, onSessionReady = { session ->
            SwingUtilities.invokeLater {
                currentSession = session
                replaceSelectedSession(session.id)
            }
        }).whenComplete { session, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("AtomCode: ${error.cause?.message ?: error.message ?: "failed"}\n")
                    finishPromptAndContinue()
                } else if (session != null) {
                    currentSession = session
                    replaceSelectedSession(session.id)
                }
            }
        }
    }

    private fun renderChatEvent(event: ChatEvent, assistantStarted: Boolean): Boolean {
        var hasAssistantOutput = assistantStarted
        fun ensureAssistantPrefix() {
            if (!hasAssistantOutput) {
                append("AtomCode: ")
                hasAssistantOutput = true
            }
        }

        when (event) {
            is ChatEvent.Text -> {
                ensureAssistantPrefix()
                append(event.content)
                currentAssistantText.append(event.content)
            }
            is ChatEvent.Reasoning -> {
                ensureAssistantPrefix()
                append(event.content)
                currentAssistantText.append(event.content)
            }
            is ChatEvent.ToolBatch -> append("\n[Tools queued]\n")
            is ChatEvent.ToolStart -> append("\n[Tool] ${event.name} started\n")
            is ChatEvent.ToolOutput -> append(event.chunk)
            is ChatEvent.ToolResult -> {
                val marker = if (event.success) "done" else "failed"
                append("\n[Tool] ${event.name} $marker in ${event.durationMs}ms\n")
            }
            is ChatEvent.ArtifactStart -> append("\n[Artifact] ${event.title ?: event.artifactType} started\n")
            is ChatEvent.ArtifactContent -> {
                append(event.content)
                currentAssistantText.append(event.content)
            }
            is ChatEvent.ArtifactEnd -> append("\n[Artifact] ${event.id} ended\n")
            is ChatEvent.PermissionRequest -> {
                append("\n[Permission required] ${event.toolName}: ${event.reason}\n")
                requestPermissionDecision(event)
            }
            is ChatEvent.Tokens -> append("\n[Tokens] prompt=${event.prompt}, completion=${event.completion}, total=${event.total}\n")
            is ChatEvent.Done -> append("\n[Done] tokens=${event.tokens}, tools=${event.toolCalls}\n")
            ChatEvent.Stopped -> append("\n[Stopped]\n")
            is ChatEvent.Error -> append("\n[Error] ${event.message}\n")
            is ChatEvent.Unknown -> append("\n[Unknown event] ${event.type}\n")
        }
        return hasAssistantOutput
    }

    private fun isTerminalEvent(event: ChatEvent): Boolean =
        event is ChatEvent.Done || event is ChatEvent.Error || event == ChatEvent.Stopped

    private fun finishPromptAndContinue() {
        finishPrompt()
        val next = if (queuedPrompts.isEmpty()) null else queuedPrompts.removeFirst()
        if (next == null) {
            renderQueueState()
            return
        }
        append("\n[Sending queued message]\n")
        startPrompt(next.prompt, next.message, next.contextNames)
    }

    private fun finishPrompt() {
        if (!generating) return
        generating = false
        if (currentAssistantText.isNotBlank()) {
            lastAssistantText = currentAssistantText.toString()
            currentAssistantText.setLength(0)
            renderAssistantActions()
        }
        send.isEnabled = true
        send.text = "Send"
        stop.isEnabled = false
        input.requestFocusInWindow()
        renderQueueState()
    }

    private fun copyLastAssistantResponse() {
        if (lastAssistantText.isBlank()) return
        CopyPasteManager.getInstance().setContents(StringSelection(lastAssistantText))
        append("AtomCode: Copied last response.\n")
    }

    private fun applyLastCodeBlock() {
        val code = extractLastCodeBlock(lastAssistantText)
        if (code.isNullOrBlank()) {
            Messages.showWarningDialog(project, "No code block found in the last AtomCode response.", "AtomCode")
            return
        }
        val editor = FileEditorManager.getInstance(project).selectedTextEditor
        if (editor == null) {
            Messages.showWarningDialog(project, "Open an editor file before applying code.", "AtomCode")
            return
        }

        val document = editor.document
        val selection = editor.selectionModel
        val start = if (selection.hasSelection()) selection.selectionStart else editor.caretModel.offset
        val end = if (selection.hasSelection()) selection.selectionEnd else editor.caretModel.offset
        val before = document.text
        val after = before.replaceRange(start, end, code)
        val contentFactory = DiffContentFactory.getInstance()
        val request = SimpleDiffRequest(
            "AtomCode Apply Code Preview",
            contentFactory.create(before),
            contentFactory.create(after),
            "Current editor",
            "After AtomCode",
        )
        DiffManager.getInstance().showDiff(project, request)

        val choice = Messages.showYesNoDialog(
            project,
            "Apply the previewed AtomCode code block to the active editor?",
            "AtomCode",
            Messages.getQuestionIcon(),
        )
        if (choice != Messages.YES) {
            append("AtomCode: Apply Code cancelled after preview.\n")
            return
        }

        WriteCommandAction.runWriteCommandAction(project, "Apply AtomCode Code", null, Runnable {
            if (selection.hasSelection()) {
                document.replaceString(selection.selectionStart, selection.selectionEnd, code)
                selection.removeSelection()
            } else {
                document.insertString(editor.caretModel.offset, code)
            }
        })
        append("AtomCode: Applied the last code block to the active editor.\n")
    }

    private fun renderAssistantActions() {
        // state is tracked via lastAssistantText; menu items check it when clicked
    }

    private fun renderQueueState() {
        rebuildContextChips()
    }

    private fun requestPermissionDecision(event: ChatEvent.PermissionRequest) {
        val args = event.arguments.take(1200)
        val message = buildString {
            appendLine("AtomCode wants to run a tool.")
            appendLine()
            appendLine("Tool: ${event.toolName}")
            if (event.reason.isNotBlank()) {
                appendLine("Reason: ${event.reason}")
            }
            if (args.isNotBlank()) {
                appendLine()
                appendLine(args)
            }
        }
        val choice = Messages.showDialog(
            this,
            message,
            "AtomCode Tool Permission",
            arrayOf("Allow Once", "Deny", "Always Allow"),
            0,
            Messages.getWarningIcon(),
        )
        val decision = when (choice) {
            0 -> "allow"
            2 -> "allow_persist"
            else -> "deny"
        }
        append("[Permission] $decision\n")
        service.respondToPermission(event.sessionId, decision, event.toolName).whenComplete { ok, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    append("[Permission error] ${error.cause?.message ?: error.message ?: "failed"}\n")
                } else if (ok != true) {
                    append("[Permission error] no pending permission for this session\n")
                }
            }
        }
    }

    private fun renderConnectionState(state: ConnectionState) {
        status.text = when (state) {
            ConnectionState.Idle -> "○ Idle"
            ConnectionState.CheckingDaemon -> "◌ Checking..."
            is ConnectionState.SetupRequired -> "○ Setup required"
            ConnectionState.StartingDaemon -> "◌ Starting..."
            ConnectionState.Connecting -> "◌ Connecting..."
            ConnectionState.SyncingProject -> "◌ Syncing..."
            ConnectionState.CheckingProvider -> "◌ Checking..."
            is ConnectionState.ProviderMissing -> "○ No provider"
            is ConnectionState.Ready -> "● Connected"
            is ConnectionState.Error -> "○ Error"
        }
        status.toolTipText = when (state) {
            is ConnectionState.Ready -> "AtomCode ${state.daemonVersion} — Click to reconnect"
            is ConnectionState.Error -> state.message
            is ConnectionState.SetupRequired -> state.reason
            else -> null
        }
    }

    private fun installInputKeyBindings() {
        input.inputMap.put(KeyStroke.getKeyStroke("ENTER"), "atomcode-enter")
        input.actionMap.put("atomcode-enter", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (settings.state.sendWithCtrlEnter) {
                    input.replaceSelection("\n")
                } else {
                    sendPrompt()
                }
            }
        })
        input.inputMap.put(KeyStroke.getKeyStroke("ctrl ENTER"), "atomcode-ctrl-enter")
        input.actionMap.put("atomcode-ctrl-enter", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (settings.state.sendWithCtrlEnter) {
                    sendPrompt()
                } else {
                    input.replaceSelection("\n")
                }
            }
        })
    }

    private fun applyChatSettings() {
        val size = settings.state.chatFontSize
        messages.font = messages.font.deriveFont(size.toFloat())
        input.font = input.font.deriveFont(size.toFloat())
    }

    private fun append(text: String) {
        messages.append(text)
        messages.caretPosition = messages.document.length
    }

    private fun clearPendingContext() {
        pendingContext.clear()
        rebuildContextChips()
    }

    private fun rebuildContextChips() {
        contextChips.removeAll()
        if (pendingContext.isEmpty() && queuedPrompts.isEmpty()) {
            contextChips.isVisible = false
            return
        }
        contextChips.isVisible = true
        if (queuedPrompts.isNotEmpty()) {
            contextChips.add(JLabel("${queuedPrompts.size} queued"))
        }
        pendingContext.forEach { item ->
            val chip = JPanel(BorderLayout(4, 0)).apply {
                border = BorderFactory.createCompoundBorder(
                    BorderFactory.createLineBorder(java.awt.Color.GRAY, 1, true),
                    BorderFactory.createEmptyBorder(1, 4, 1, 2),
                )
                toolTipText = item.path
            }
            val label = "📄 " + item.displayName + (if (item.startLine != null) " (L${item.startLine}-${item.endLine})" else "")
            chip.add(JLabel(label), BorderLayout.CENTER)
            val removeBtn = JButton("×").apply {
                isContentAreaFilled = false
                isBorderPainted = false
                font = font.deriveFont(font.size2D + 2f)
                addActionListener {
                    pendingContext.remove(item)
                    rebuildContextChips()
                }
            }
            chip.add(removeBtn, BorderLayout.EAST)
            contextChips.add(chip)
        }
        if (pendingContext.isNotEmpty()) {
            contextChips.add(clearContext.apply { isEnabled = true })
        }
        contextChips.revalidate()
        contextChips.repaint()
    }

    private fun buildPromptWithContext(prompt: String, context: List<ChatContextItem>): String {
        if (context.isEmpty()) return prompt
        return buildString {
            appendLine("The user has attached the following file(s)/selection(s) for context. The content is provided inline below - DO NOT use read_file to re-read them.")
            appendLine()
            context.forEach { item ->
                val location = if (item.startLine != null && item.endLine != null) {
                    " (lines ${item.startLine}-${item.endLine})"
                } else {
                    ""
                }
                appendLine("File: ${item.displayName}$location")
                appendLine("```${item.language}")
                appendLine(item.content)
                appendLine("```")
                appendLine()
            }
            append("User question: ")
            append(prompt)
        }
    }

    private fun buildAutomaticContext(existingContext: List<ChatContextItem>): List<ChatContextItem> {
        val level = settings.state.contextLevel
        if (level == AtomCodeContextLevel.Minimal) return emptyList()

        val result = mutableListOf<ChatContextItem>()
        if (level == AtomCodeContextLevel.ProjectContext) {
            result += ChatContextItem(
                path = project.basePath.orEmpty(),
                displayName = "Project context",
                language = "text",
                content = buildString {
                    appendLine("Project: ${project.name}")
                    project.basePath?.let { appendLine("Base path: $it") }
                    currentSession?.id?.let { appendLine("AtomCode session: $it") }
                }.trimEnd(),
                selection = null,
                startLine = null,
                endLine = null,
            )
        }

        val editor = FileEditorManager.getInstance(project).selectedTextEditor ?: return result
        val virtualFile = FileDocumentManager.getInstance().getFile(editor.document) ?: return result
        if (existingContext.any { it.path == virtualFile.path } || result.any { it.path == virtualFile.path }) {
            return result
        }

        val path = virtualFile.path
        when (SensitivePathClassifier.classify(path)) {
            PathSensitivity.Block,
            PathSensitivity.StrongConfirm -> {
                append("AtomCode: Skipped automatic context for sensitive file ${virtualFile.name}.\n")
                return result
            }
            PathSensitivity.Warn,
            PathSensitivity.Normal -> Unit
        }

        if (settings.state.autoSaveBeforeRead) {
            com.intellij.openapi.application.WriteIntentReadAction.run {
                FileDocumentManager.getInstance().saveAllDocuments()
            }
        }
        val content = editor.document.text
        if (content.isBlank()) return result
        if (content.length > MAX_ATTACHED_FILE_CHARS) {
            append("AtomCode: Skipped automatic context for ${virtualFile.name}; file is too large.\n")
            return result
        }

        val relative = project.basePath?.let { base ->
            if (path.startsWith(base)) path.removePrefix(base).trimStart('/', '\\') else path
        } ?: path
        val displayName = if (settings.state.sendRelativePathWithSelection) relative else path
        result += ChatContextItem(
            path = path,
            displayName = displayName,
            language = virtualFile.extension ?: "text",
            content = content,
            selection = null,
            startLine = null,
            endLine = null,
        )
        return result
    }

    internal fun showGearMenu() {
        val menu = JPopupMenu()

        // Connect / Start
        menu.add(JMenuItem("🔌 Connect / Start").apply {
            addActionListener { connect() }
        })

        menu.add(JSeparator())

        // Provider submenu
        val providerMenu = JMenu("Provider ▸")
        providerMenu.add(JMenuItem("Create Provider...").apply {
            addActionListener { showCreateProviderDialog() }
        })
        providerMenu.add(JMenuItem("Edit Provider...").apply {
            addActionListener { showEditProviderDialog() }
        })
        providerMenu.add(JMenuItem("Delete Provider...").apply {
            addActionListener { deleteSelectedProvider() }
        })
        providerMenu.add(JSeparator())
        providerMenu.add(JMenuItem("Thinking Settings...").apply {
            addActionListener { showThinkingDialog() }
        })
        menu.add(providerMenu)

        menu.add(JSeparator())

        // Auth & Setup
        menu.add(JMenuItem("🔑 Login").apply {
            addActionListener { login() }
        })
        menu.add(JMenuItem("🚀 CodingPlan Setup").apply {
            addActionListener { runSetup() }
        })

        menu.add(JSeparator())

        // Tab management
        menu.add(JMenuItem("➕ New Chat Tab").apply {
            addActionListener { openAtomCodeChatTab(project, newTab = true) }
        })
        menu.add(JMenuItem("✖ Close Current Tab").apply {
            addActionListener { closeCurrentChatTab(project) }
        })

        menu.add(JSeparator())

        // Session management
        menu.add(JMenuItem("📋 Session History...").apply {
            addActionListener { showSessionHistory() }
        })
        menu.add(JMenuItem("✏️ Rename Session").apply {
            addActionListener { renameSelectedSession() }
        })
        menu.add(JMenuItem("🗑 Delete Session").apply {
            addActionListener { deleteSelectedSession() }
        })
        menu.add(JMenuItem("🔄 Refresh Sessions").apply {
            addActionListener { refreshSessionList() }
        })

        menu.add(JSeparator())

        // Slash commands
        menu.add(JMenuItem("💬 Slash Commands...").apply {
            addActionListener { showCommandMenu() }
        })

        menu.add(JSeparator())

        // Tools
        menu.add(JMenuItem("📋 Copy Last Response").apply {
            addActionListener { copyLastAssistantResponse() }
        })
        menu.add(JMenuItem("📝 Apply Last Code Block").apply {
            addActionListener { applyLastCodeBlock() }
        })
        menu.add(JSeparator())
        menu.add(JMenuItem("📂 Open Changes").apply {
            addActionListener { openProjectChanges() }
        })
        menu.add(JMenuItem("🩺 Diagnostics").apply {
            addActionListener { showDiagnostics() }
        })
        menu.add(JMenuItem("⚙ Settings...").apply {
            addActionListener { project.openAtomCodeSettings() }
        })

        val pointer = java.awt.MouseInfo.getPointerInfo().location
        SwingUtilities.convertPointFromScreen(pointer, this)
        menu.show(this, pointer.x, pointer.y)
    }

    private fun showCommandMenu() {
        val menu = JPopupMenu()
        val items = listOf(
            SlashCommand("/login", "Sign in with AtomGit"),
            SlashCommand("/codingplan", "Sync CodingPlan models"),
            SlashCommand("/explain", "Explain code"),
            SlashCommand("/fix", "Fix issues"),
            SlashCommand("/test", "Write tests"),
            SlashCommand("/refactor", "Refactor code"),
            SlashCommand("/docs", "Add documentation"),
            SlashCommand("/review", "Review code"),
            SlashCommand("/optimize", "Optimize performance"),
        )
        items.forEach { command ->
            menu.add(JMenuItem("${command.name} - ${command.description}").apply {
                addActionListener {
                    input.text = "${command.name} "
                    input.requestFocusInWindow()
                }
            })
        }
        val pointer = java.awt.MouseInfo.getPointerInfo().location
        SwingUtilities.convertPointFromScreen(pointer, this)
        menu.show(this, pointer.x, pointer.y)
    }

    private fun chooseFilesForContext() {
        val descriptor = FileChooserDescriptor(
            true,
            false,
            false,
            false,
            false,
            true,
        ).withTitle("Attach Files to AtomCode")

        val projectDir = project.basePath?.let {
            LocalFileSystem.getInstance().refreshAndFindFileByPath(it)
        }
        val files = FileChooser.chooseFiles(descriptor, project, projectDir)
        if (files.isEmpty()) return

        files.forEach(::attachVirtualFile)
    }

    private fun attachVirtualFile(file: VirtualFile) {
        val path = file.path
        when (SensitivePathClassifier.classify(path)) {
            PathSensitivity.Block -> {
                Messages.showWarningDialog(project, "AtomCode will not attach this sensitive file.", "AtomCode")
                return
            }
            PathSensitivity.StrongConfirm -> {
                val choice = Messages.showYesNoDialog(
                    project,
                    "This file may contain sensitive information. Attach it to the next AtomCode message?",
                    "AtomCode",
                    Messages.getWarningIcon(),
                )
                if (choice != Messages.YES) return
            }
            PathSensitivity.Warn,
            PathSensitivity.Normal -> Unit
        }

        if (settings.state.autoSaveBeforeRead) {
            com.intellij.openapi.application.WriteIntentReadAction.run {
                FileDocumentManager.getInstance().saveAllDocuments()
            }
            file.refresh(false, false)
        }

        val content = try {
            String(file.contentsToByteArray(), Charsets.UTF_8)
        } catch (error: Exception) {
            Messages.showWarningDialog(project, "Could not read ${file.name}: ${error.message}", "AtomCode")
            return
        }

        if (content.isBlank()) return
        if (content.length > MAX_ATTACHED_FILE_CHARS) {
            Messages.showWarningDialog(project, "This file is too large to attach. Select a smaller file or attach a selection.", "AtomCode")
            return
        }

        val relative = project.basePath?.let { base ->
            if (path.startsWith(base)) path.removePrefix(base).trimStart('/', '\\') else path
        } ?: path

        addContext(
            ChatContextItem(
                path = path,
                displayName = relative,
                language = file.extension ?: "text",
                content = content,
                selection = null,
                startLine = null,
                endLine = null,
            ),
        )
        append("AtomCode: Attached $relative.\n")
    }

    private fun handleLocalInputCommand(prompt: String): Boolean {
        val command = prompt.split(Regex("\\s+"), limit = 2).firstOrNull()?.lowercase() ?: return false
        return when (command) {
            "/login" -> {
                append("AtomCode: Opening AtomGit sign-in in your browser. Complete authorization there, then return to the IDE.\n")
                login()
                true
            }
            "/codingplan" -> {
                append("AtomCode: Running CodingPlan setup...\n")
                runSetup()
                true
            }
            else -> false
        }
    }

    private fun slashPromptTemplate(prompt: String): String? =
        com.atomcode.jetbrains.ui.slashPromptTemplate(prompt)

    private fun extractLastCodeBlock(text: String): String? =
        com.atomcode.jetbrains.ui.extractLastCodeBlock(text)
}

private data class SlashCommand(
    val name: String,
    val description: String,
)

private data class QueuedPrompt(
    val prompt: String,
    val message: String,
    val contextNames: List<String>,
)

data class ChatContextItem(
    val path: String,
    val displayName: String,
    val language: String,
    val content: String,
    val selection: String?,
    val startLine: Int?,
    val endLine: Int?,
)

internal fun slashPromptTemplate(prompt: String): String? {
    val parts = prompt.split(Regex("\\s+"), limit = 2)
    val command = parts.firstOrNull()?.lowercase() ?: return null
    val suffix = parts.getOrNull(1)?.trim().orEmpty()
    val template = when (command) {
        "/explain" -> "Please explain this code. What does it do, and why?"
        "/fix" -> "Please fix any bugs or issues in this code."
        "/test" -> "Please write tests for this code."
        "/refactor" -> "Please refactor this code for better readability and maintainability."
        "/docs" -> "Please add documentation comments to this code."
        "/review" -> "Please review this code for issues, improvements, and best practices."
        "/optimize" -> "Please optimize this code for better performance and readability."
        else -> return null
    }
    return if (suffix.isBlank()) template else "$template\n\n$suffix"
}

internal fun extractLastCodeBlock(text: String): String? {
    val matches = Regex("""```[^\n`]*
([\s\S]*?)```""").findAll(text).toList()
    return matches.lastOrNull()?.groupValues?.getOrNull(1)?.trimEnd()
}
