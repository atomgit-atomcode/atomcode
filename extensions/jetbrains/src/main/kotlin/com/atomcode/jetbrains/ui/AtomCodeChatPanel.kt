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
import com.atomcode.jetbrains.session.ChatRuntime
import com.atomcode.jetbrains.session.ContextItemState
import com.atomcode.jetbrains.session.SessionWorkspace
import com.atomcode.jetbrains.settings.AtomCodeContextLevel
import com.atomcode.jetbrains.settings.AtomCodeSettingsState
import com.atomcode.jetbrains.ui.header.HeaderPanel
import com.atomcode.jetbrains.ui.input.InputPanel
import com.atomcode.jetbrains.ui.input.QueuedPromptView
import com.atomcode.jetbrains.ui.message.JBCefMessageView
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
import java.util.UUID
import javax.swing.DefaultListModel
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
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.JSeparator
import javax.swing.ListSelectionModel
import javax.swing.JOptionPane
import javax.swing.SwingUtilities
import javax.swing.Timer
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

private const val MAX_ATTACHED_FILE_CHARS = 120_000

class AtomCodeChatPanel(
    private val project: Project,
    private val runtime: ChatRuntime? = null,
) : JPanel(BorderLayout()), Disposable {
    private val service = AtomCodeProjectService.getInstance(project)
    private val settings = AtomCodeSettingsState.getInstance()

    // ── New UI components ──
    private val header = HeaderPanel()
    private val messageView = JBCefMessageView()
    private val inputPanel = InputPanel(
        onSend = { text -> handleSend(text) },
        onStop = { stopCurrentGeneration() },
        onAttach = { chooseFilesForContext() },
        onSlashCommand = { showCommandMenu() },
        onClearContext = { clearPendingContext() },
        onRemoveContext = { item -> pendingContext.remove(item); rebuildContext() },
        onModelSelect = { showModelPickerPopup() },
    )

    // ── Data state (preserved from original) ──
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
    private val streamHandler = StreamEventHandler(messageView)
    private val pendingContext = mutableListOf<ChatContextItem>()
    private val queuedPrompts = ArrayDeque<QueuedPrompt>()
    private var disposed = false
    private val connectionListener = java.beans.PropertyChangeListener { event: PropertyChangeEvent ->
        if (disposed) return@PropertyChangeListener
        SwingUtilities.invokeLater {
            if (!disposed) {
                renderConnectionState(event.newValue as ConnectionState)
                if (event.newValue is ConnectionState.Ready) {
                    refreshSetupSnapshot()
                    refreshSessionList()
                }
            }
        }
    }

    init {
        minimumSize = Dimension(280, 300)

        // ── Assemble 3-zone layout ──
        add(header, BorderLayout.NORTH)
        add(messageView, BorderLayout.CENTER)
        add(inputPanel, BorderLayout.SOUTH)

        // ── Action bindings ──
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
        installInputKeyBindings()

        service.addConnectionListener(connectionListener)
        renderConnectionState(service.connectionState)
        applyChatSettings()

        refreshAfterConnect()
    }

    override fun dispose() {
        if (disposed) return
        disposed = true
        queuedPrompts.clear()
        pendingContext.clear()
        service.removeConnectionListener(connectionListener)
    }

    fun focusInput() {
        inputPanel.focusInput()
    }

    fun submitPrompt(prompt: String) {
        inputPanel.setInputText(prompt)
        handleSend(prompt)
    }

    fun stopCurrentGeneration() {
        queuedPrompts.clear()
        renderQueueState()
        messageView.finishAssistantTurn()
        if (!generating) {
            service.stopGeneration(currentSession?.id)
            return
        }
        addSystemMessage("[Stopping]")
        service.stopGeneration(currentSession?.id).whenComplete { _, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    addErrorMessage("Stop failed: ${error.cause?.message ?: error.message ?: "failed"}")
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
            runtime?.addContext(item.toContextItemState())
        }
        rebuildContext()
        focusInput()
    }

    // ── Connection ──

    private fun connect() {
        header.updateConnectionState(ConnectionState.CheckingDaemon)
        refreshAfterConnect()
    }

    private fun refreshAfterConnect() {
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
                    addErrorMessage(error.cause?.message ?: error.message ?: "failed to load setup")
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

        // Update input panel model name
        val currentModel = snapshot.models.firstOrNull { it.isDefault }?.model
            ?: snapshot.currentModel.ifBlank { null }
            ?: "No model"
        inputPanel.setModelName(currentModel)
    }

    private fun login() {
        service.loginWithBrowser { message ->
            SwingUtilities.invokeLater {
                header.updateConnectionState(ConnectionState.CheckingDaemon)
            }
        }.whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    addErrorMessage("Login failed: ${error.cause?.message ?: error.message ?: "failed"}")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                addSystemMessage("Login complete.")
            }
        }
    }

    private fun setDefaultModel(model: ModelInfo) {
        modelPicker.isEnabled = false
        service.setDefaultModel(model).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                modelPicker.isEnabled = true
                if (error != null) {
                    addErrorMessage(error.cause?.message ?: error.message ?: "failed to set default model")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                addSystemMessage("Default model set to ${model.model}.")
            }
        }
    }

    private fun runSetup() {
        header.updateConnectionState(ConnectionState.CheckingDaemon)
        service.setupCodingPlan().whenComplete { report, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    addErrorMessage("Setup failed: ${error.cause?.message ?: error.message ?: "failed"}")
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                addSystemMessage("Setup:\n$report")
                refreshSetupSnapshot()
            }
        }
    }

    // ── Provider dialogs (unchanged logic) ──

    private fun showCreateProviderDialog() {
        val name = JTextField("default")
        val type = JComboBox(arrayOf("openai", "claude", "ollama"))
        val model = JTextField("gpt-4o-mini")
        val apiKey = JPasswordField()
        val baseUrl = JTextField()
        val setDefault = JCheckBox("Set as default", true)

        val form = JPanel(GridBagLayout())
        fun addRow(row: Int, label: String, field: java.awt.Component) {
            form.add(JLabel(label), GridBagConstraints().apply { gridx = 0; gridy = row; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 8) })
            form.add(field, GridBagConstraints().apply { gridx = 1; gridy = row; weightx = 1.0; fill = GridBagConstraints.HORIZONTAL; insets = Insets(4, 4, 4, 4) })
        }
        addRow(0, "Name", name)
        addRow(1, "Type", type)
        addRow(2, "Model", model)
        addRow(3, "API Key", apiKey)
        addRow(4, "Base URL", baseUrl)
        form.add(setDefault, GridBagConstraints().apply { gridx = 1; gridy = 5; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 4) })

        val choice = JOptionPane.showConfirmDialog(this, form, "Create AtomCode Provider", JOptionPane.OK_CANCEL_OPTION, JOptionPane.PLAIN_MESSAGE)
        if (choice != JOptionPane.OK_OPTION) return

        val request = CreateProviderRequest(
            name = name.text.trim(), type = (type.selectedItem as? String).orEmpty(),
            model = model.text.trim(), apiKey = String(apiKey.password).trim().ifBlank { null },
            baseUrl = baseUrl.text.trim().ifBlank { null }, setDefault = setDefault.isSelected,
        )
        if (request.name.isBlank() || request.type.isBlank() || request.model.isBlank()) {
            Messages.showWarningDialog(this, "Name, type, and model are required.", "AtomCode"); return
        }
        service.createProvider(request).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage("Provider failed: ${error.cause?.message ?: error.message ?: "failed"}"); refreshSetupSnapshot(); return@invokeLater }
                renderSetupSnapshot(snapshot); addSystemMessage("Provider ${request.name} saved.")
            }
        }
    }

    private fun showEditProviderDialog() {
        val selected = selectedProvider() ?: return
        val name = JTextField(selected.name)
        val type = JComboBox(arrayOf("openai", "claude", "ollama")).apply { selectedItem = selected.type.ifBlank { "openai" } }
        val model = JTextField(selected.model)
        val apiKey = JPasswordField()
        val clearApiKey = JCheckBox("Clear API key", false)
        val baseUrl = JTextField()
        val clearBaseUrl = JCheckBox("Clear Base URL", false)

        val form = JPanel(GridBagLayout())
        fun addRow(row: Int, label: String, field: java.awt.Component) {
            form.add(JLabel(label), GridBagConstraints().apply { gridx = 0; gridy = row; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 8) })
            form.add(field, GridBagConstraints().apply { gridx = 1; gridy = row; weightx = 1.0; fill = GridBagConstraints.HORIZONTAL; insets = Insets(4, 4, 4, 4) })
        }
        addRow(0, "Name", name); addRow(1, "Type", type); addRow(2, "Model", model)
        addRow(3, "New API Key", apiKey); addRow(4, "Base URL", baseUrl)
        form.add(clearApiKey, GridBagConstraints().apply { gridx = 1; gridy = 5; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 4) })
        form.add(clearBaseUrl, GridBagConstraints().apply { gridx = 1; gridy = 6; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 4) })

        val choice = JOptionPane.showConfirmDialog(this, form, "Edit AtomCode Provider", JOptionPane.OK_CANCEL_OPTION, JOptionPane.PLAIN_MESSAGE)
        if (choice != JOptionPane.OK_OPTION) return

        val request = PatchProviderRequest(
            originalName = selected.name, name = name.text.trim(), type = (type.selectedItem as? String).orEmpty(),
            model = model.text.trim(), apiKey = String(apiKey.password).trim().ifBlank { null },
            clearApiKey = clearApiKey.isSelected, baseUrl = baseUrl.text.trim().ifBlank { null },
            clearBaseUrl = clearBaseUrl.isSelected,
        )
        if (request.name.isBlank() || request.type.isBlank() || request.model.isBlank()) {
            Messages.showWarningDialog(this, "Name, type, and model are required.", "AtomCode"); return
        }
        service.patchProvider(request).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage("Provider update failed: ${error.cause?.message ?: error.message ?: "failed"}"); refreshSetupSnapshot(); return@invokeLater }
                renderSetupSnapshot(snapshot); addSystemMessage("Provider ${request.name} updated.")
            }
        }
    }

    private fun deleteSelectedProvider() {
        val selected = selectedProvider() ?: return
        val choice = Messages.showYesNoDialog(this, "Delete provider \"${selected.name}\" from AtomCode config?", "AtomCode", Messages.getWarningIcon())
        if (choice != Messages.YES) return
        service.deleteProvider(selected.name).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage("Provider delete failed: ${error.cause?.message ?: error.message ?: "failed"}"); refreshSetupSnapshot(); return@invokeLater }
                renderSetupSnapshot(snapshot); addSystemMessage("Provider ${selected.name} deleted.")
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
            form.add(JLabel(label), GridBagConstraints().apply { gridx = 0; gridy = row; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 8) })
            form.add(field, GridBagConstraints().apply { gridx = 1; gridy = row; weightx = 1.0; fill = GridBagConstraints.HORIZONTAL; insets = Insets(4, 4, 4, 4) })
        }
        form.add(enabled, GridBagConstraints().apply { gridx = 1; gridy = 0; anchor = GridBagConstraints.WEST; insets = Insets(4, 4, 4, 4) })
        addRow(1, "Budget", budget); addRow(2, "Type", type); addRow(3, "Keep", keep)

        val choice = JOptionPane.showConfirmDialog(this, form, "AtomCode Thinking - ${selected.name}", JOptionPane.OK_CANCEL_OPTION, JOptionPane.PLAIN_MESSAGE)
        if (choice != JOptionPane.OK_OPTION) return

        val budgetValue = budget.text.trim().takeIf { it.isNotBlank() }?.toIntOrNull()
        if (budget.text.trim().isNotBlank() && budgetValue == null) { Messages.showWarningDialog(this, "Thinking budget must be a number.", "AtomCode"); return }
        service.patchProviderThinking(selected.name, PatchThinkingRequest(enabled = enabled.isSelected, budget = budgetValue, type = type.text.trim().ifBlank { null }, keep = keep.text.trim().ifBlank { null })).whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage("Thinking update failed: ${error.cause?.message ?: error.message ?: "failed"}"); refreshSetupSnapshot(); return@invokeLater }
                renderSetupSnapshot(snapshot)
                val state = if (enabled.isSelected) "enabled" else "disabled"; addSystemMessage("Thinking $state for ${selected.name}.")
            }
        }
    }

    private fun selectedProvider(): ProviderInfo? {
        val selectedModel = modelPicker.selectedItem as? ModelInfo
        val snapshot = setupSnapshot ?: return null
        return selectedModel?.let { model -> snapshot.providers.firstOrNull { it.name == model.provider } }
            ?: snapshot.providers.firstOrNull { it.isDefault } ?: snapshot.providers.firstOrNull()
    }

    // ── Session management ──

    fun startNewConversation() {
        service.createSession().whenComplete { session, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to create session"); return@invokeLater }
                currentSession = session
                runtime?.updateSession(session)
                persistRuntimeSession()
                messageView.clear()
                addSystemMessage("Started new session ${session.name.ifBlank { session.id.take(8) }}.")
                refreshSessionList()
                inputPanel.focusInput()
            }
        }
    }

    private fun showSessionHistory() {
        service.refreshSessions().whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to load sessions"); return@invokeLater }
                openSessionHistoryDialog(sessions)
            }
        }
    }

    private fun openSessionHistoryDialog(initialSessions: List<SessionMeta>) {
        var sessions = initialSessions.sortedByDescending { it.updatedAt }
        val model = DefaultListModel<SessionMeta>()
        val search = JTextField()
        val list = JList(model).apply { selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION; visibleRowCount = 14 }
        val load = JButton("Load"); val rename = JButton("Rename"); val delete = JButton("Delete Selected")
        val refresh = JButton("Refresh"); val close = JButton("Close")
        var searchGeneration = 0

        fun updateHistoryButtons() {
            val selectedCount = list.selectedValuesList.size
            load.isEnabled = selectedCount == 1; rename.isEnabled = selectedCount == 1; delete.isEnabled = selectedCount > 0
        }
        fun refill(items: List<SessionMeta> = sessions) {
            model.clear()
            items.forEach(model::addElement)
            val hasItems = model.size() > 0
            if (hasItems && list.selectedIndex < 0) list.selectedIndex = 0
            updateHistoryButtons()
        }
        fun runSearch() {
            val query = search.text.trim()
            val generation = ++searchGeneration
            val future = if (query.isBlank()) service.refreshSessions() else service.searchSessions(query)
            future.whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    if (generation != searchGeneration) return@invokeLater
                    if (error != null) {
                        addErrorMessage(error.cause?.message ?: error.message ?: "failed to search sessions")
                        return@invokeLater
                    }
                    sessions = updated.sortedByDescending { it.updatedAt }
                    replaceSessions(sessions, currentSession?.id)
                    refill(sessions)
                }
            }
        }
        val searchTimer = Timer(300) { runSearch() }.apply { isRepeats = false }
        list.addListSelectionListener { if (!it.valueIsAdjusting) updateHistoryButtons() }
        search.document.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent) = searchTimer.restart()
            override fun removeUpdate(e: DocumentEvent) = searchTimer.restart()
            override fun changedUpdate(e: DocumentEvent) = searchTimer.restart()
        })

        val panel = JPanel(BorderLayout(8, 8)).apply {
            add(search, BorderLayout.NORTH); add(JScrollPane(list), BorderLayout.CENTER)
            add(JPanel().apply { add(load); add(rename); add(delete); add(refresh); add(close) }, BorderLayout.SOUTH)
            preferredSize = Dimension(560, 360)
        }
        val dialog = JDialog(SwingUtilities.getWindowAncestor(this), "AtomCode Session History", Dialog.ModalityType.APPLICATION_MODAL).apply {
            contentPane = panel; pack(); setLocationRelativeTo(this@AtomCodeChatPanel)
        }
        load.addActionListener { val selected = list.selectedValue ?: return@addActionListener; dialog.dispose(); loadSession(selected) }
        rename.addActionListener {
            val selected = list.selectedValue ?: return@addActionListener
            val nextName = JOptionPane.showInputDialog(dialog, "Session name", selected.displayName)?.trim() ?: return@addActionListener
            if (nextName.isBlank()) { Messages.showWarningDialog(dialog, "Session name cannot be empty.", "AtomCode"); return@addActionListener }
            rename.isEnabled = false
            service.renameSession(selected, nextName).whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    rename.isEnabled = true
                    if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to rename session"); return@invokeLater }
                    sessions = updated.sortedByDescending { it.updatedAt }; replaceSessions(sessions, selected.id); refill(sessions)
                    addSystemMessage("Session renamed to $nextName.")
                }
            }
        }
        delete.addActionListener {
            val selected = list.selectedValuesList; if (selected.isEmpty()) return@addActionListener
            val label = if (selected.size == 1) "Delete AtomCode session \"${selected.first().displayName}\" from local history?" else "Delete ${selected.size} AtomCode sessions from local history?"
            val choice = Messages.showYesNoDialog(dialog, label, "AtomCode", Messages.getWarningIcon())
            if (choice != Messages.YES) return@addActionListener
            delete.isEnabled = false
            service.deleteSessions(selected).whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    delete.isEnabled = true
                    if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to delete sessions"); return@invokeLater }
                    sessions = updated.sortedByDescending { it.updatedAt }
                    if (selected.any { it.id == currentSession?.id }) { currentSession = null }
                    replaceSessions(sessions, currentSession?.id)
                    if (currentSession == null) messageView.clear()
                    refill(sessions); addSystemMessage("Deleted ${selected.size} session(s).")
                }
            }
        }
        refresh.addActionListener {
            refresh.isEnabled = false
            service.refreshSessions().whenComplete { updated, error ->
                SwingUtilities.invokeLater {
                    refresh.isEnabled = true
                    if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to refresh sessions"); return@invokeLater }
                    sessions = updated.sortedByDescending { it.updatedAt }; replaceSessions(sessions, currentSession?.id); refill(sessions)
                }
            }
        }
        close.addActionListener { dialog.dispose() }
        refill(sessions); dialog.isVisible = true
    }

    private fun renameSelectedSession() {
        val selected = sessionPicker.selectedItem as? SessionMeta ?: return
        val nextName = JOptionPane.showInputDialog(this, "Session name", selected.displayName)?.trim() ?: return
        if (nextName.isBlank()) { Messages.showWarningDialog(this, "Session name cannot be empty.", "AtomCode"); return }
        service.renameSession(selected, nextName).whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to rename session"); return@invokeLater }
                replaceSessions(sessions, selected.id); addSystemMessage("Session renamed to $nextName.")
            }
        }
    }

    private fun deleteSelectedSession() {
        val selected = sessionPicker.selectedItem as? SessionMeta ?: return
        val choice = Messages.showYesNoDialog(this, "Delete AtomCode session \"${selected.displayName}\" from local history?", "AtomCode", Messages.getWarningIcon())
        if (choice != Messages.YES) return
        service.deleteSession(selected).whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to delete session"); return@invokeLater }
                if (currentSession?.id == selected.id) { currentSession = null }
                replaceSessions(sessions, currentSession?.id)
                if (currentSession == null) messageView.clear()
                addSystemMessage("Session deleted.")
            }
        }
    }

    fun openProjectChanges() {
        service.fileChangeService.openChangedFiles().whenComplete { files, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to open changes"); service.fileChangeService.openLocalChanges(); return@invokeLater }
                if (files.isEmpty()) { addSystemMessage("No Git changes found. Opened Local Changes."); service.fileChangeService.openLocalChanges() }
                else { addSystemMessage("Opened changed files: ${files.joinToString()}") }
            }
        }
    }

    private fun showDiagnostics() {
        val snapshot = setupSnapshot; val state = settings.state
        val details = buildString {
            appendLine("Connection: ${service.connectionState}"); appendLine("Active session: ${currentSession?.id ?: "(none)"}")
            appendLine("Daemon host: ${state.host}"); appendLine("Daemon port: ${state.port}")
            appendLine("Daemon binary path: ${state.daemonBinaryPath.ifBlank { "(auto-detect)" }}")
            appendLine("Auto-start: ${state.autoStart}"); appendLine("Auto-save before read: ${state.autoSaveBeforeRead}")
            appendLine("Context level: ${state.contextLevel}"); appendLine("Allow selected text context: ${state.allowSelectedTextContext}")
            appendLine("Send relative path with selection: ${state.sendRelativePathWithSelection}")
            appendLine("Send with Ctrl+Enter: ${state.sendWithCtrlEnter}"); appendLine("Chat font size: ${state.chatFontSize}")
            appendLine("Pending context items: ${pendingContext.size}"); appendLine("Queued prompts: ${queuedPrompts.size}")
            if (snapshot != null) {
                appendLine("Setup required: ${snapshot.setupRequired}"); appendLine("Signed in: ${snapshot.auth?.loggedIn ?: false}")
                appendLine("User: ${snapshot.auth?.userName ?: "(none)"}"); appendLine("Providers: ${snapshot.providers.size}")
                appendLine("Default provider: ${snapshot.defaultProvider.ifBlank { "(none)" }}")
                appendLine("Current model: ${snapshot.currentModel.ifBlank { "(none)" }}")
            } else { appendLine("Setup snapshot: not loaded") }
        }
        val text = AtomCodeDiagnostics.summary(project, details)
        CopyPasteManager.getInstance().setContents(StringSelection(text))
        val area = JTextArea(text).apply { isEditable = false; lineWrap = false; rows = 22; columns = 72 }
        JOptionPane.showMessageDialog(this, JScrollPane(area), "AtomCode Diagnostics (copied)", JOptionPane.INFORMATION_MESSAGE)
    }

    // ── Session list ──

    private fun refreshSessionList() {
        service.refreshSessions().whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to load sessions"); return@invokeLater }
                replaceSessions(sessions, currentSession?.id)
            }
        }
    }

    private fun replaceSessions(sessions: List<SessionMeta>, selectedSessionId: String?) {
        loadingSessions = true; sessionPicker.removeAllItems(); sessions.forEach(sessionPicker::addItem)
        selectedSessionId?.let { active ->
            val match = (0 until sessionPicker.itemCount).map { sessionPicker.getItemAt(it) }.firstOrNull { it.id == active }
            if (match != null) sessionPicker.selectedItem = match
        }
        loadingSessions = false
    }

    private fun replaceSelectedSession(sessionId: String?) {
        if (sessionId == null) return
        loadingSessions = true
        val match = (0 until sessionPicker.itemCount).map { sessionPicker.getItemAt(it) }.firstOrNull { it.id == sessionId }
        if (match != null) { sessionPicker.selectedItem = match }
        loadingSessions = false
    }

    private fun loadSession(meta: SessionMeta) {
        service.loadSessionDetail(meta).whenComplete { detail, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to load session"); return@invokeLater }
                currentSession = SessionRefView(detail.id, detail.name, detail.projectHash, detail.workingDir)
                runtime?.loadSession(detail)
                persistRuntimeSession()
                replaceSelectedSession(detail.id); renderSession(detail); inputPanel.focusInput()
            }
        }
    }

    private fun renderSession(detail: SessionDetail) {
        messageView.clear()
        addSystemMessage("Loaded ${detail.name.ifBlank { detail.id.take(8) }}.\n")
        detail.messages.forEach(::renderHistoryMessage)
    }

    private fun renderHistoryMessage(message: MessageInfo) {
        val label = when (message.role) {
            "user" -> "You"; "assistant" -> "AtomCode"; "tool" -> "Tool"; "system" -> "System"; else -> message.role.ifBlank { "Message" }
        }
        addSystemMessage("$label: ${message.content}\n")
    }

    // ── Send / Chat streaming ──

    private fun handleSend(text: String) {
        val prompt = text.trim()
        if (prompt.isEmpty()) return
        if (handleLocalInputCommand(prompt)) { inputPanel.clearInput(); return }
        val transformedPrompt = slashPromptTemplate(prompt) ?: prompt
        val pendingContextForSend = pendingContext.toList()
        val contextForSend = pendingContextForSend + buildAutomaticContext(pendingContextForSend)
        val message = buildPromptWithContext(transformedPrompt, contextForSend)
        val contextNames = contextForSend.map { it.displayName }

        if (generating) {
            val queued = QueuedPrompt(UUID.randomUUID().toString(), transformedPrompt, message, contextNames)
            queuedPrompts += queued
            runtime?.queuePrompt(transformedPrompt, queued.id)
            if (pendingContextForSend.isNotEmpty()) clearPendingContext()
            inputPanel.clearInput()
            renderQueueState()
            return
        }
        if (pendingContextForSend.isNotEmpty()) clearPendingContext()
        startPrompt(transformedPrompt, message, contextNames)
    }

    private fun startPrompt(prompt: String, message: String, contextNames: List<String>) {
        // Add user message + immediate thinking feedback
        runtime?.submitPrompt(prompt)
        renderQueueState()
        messageView.addUserMessage(prompt)
        messageView.beginAssistantTurn()
        if (contextNames.isNotEmpty()) {
            addSystemMessage("[Context] ${contextNames.joinToString()}")
        }
        messageView.addThinkingIndicator()
        inputPanel.clearInput()
        generating = true
        inputPanel.setGenerating(true)
        streamHandler.reset()

        service.sendPrompt(message, currentSession, object : ChatStreamListener {
            override fun onEvent(event: ChatEvent) {
                SwingUtilities.invokeLater {
                    renderChatEvent(event)
                    if (isTerminalEvent(event)) finishPromptAndContinue()
                }
            }
            override fun onComplete() {
                SwingUtilities.invokeLater {
                    streamHandler.onComplete()
                    finishPromptAndContinue()
                }
            }
            override fun onError(message: String) {
                SwingUtilities.invokeLater { streamHandler.onError(message) }
            }
        }, onSessionReady = { session ->
            SwingUtilities.invokeLater {
                currentSession = session
                runtime?.updateSession(session)
                replaceSelectedSession(session.id)
                persistRuntimeSession()
            }
        }).whenComplete { session, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    finishPromptAndContinue()
                } else if (session != null) { currentSession = session; runtime?.updateSession(session); replaceSelectedSession(session.id); persistRuntimeSession() }
            }
        }
    }

    private fun renderChatEvent(event: ChatEvent) {
        runtime?.applyDaemonEvent(event)
        when (event) {
            is ChatEvent.Text -> streamHandler.onText(event.content)
            is ChatEvent.Reasoning -> streamHandler.onReasoning(event.content)
            is ChatEvent.ToolBatch -> streamHandler.onToolBatch()
            is ChatEvent.ToolStart -> streamHandler.onToolStart(event.name)
            is ChatEvent.ToolOutput -> streamHandler.onToolOutput(event.chunk)
            is ChatEvent.ToolResult -> streamHandler.onToolResult(event.name, event.output, event.success, event.durationMs)
            is ChatEvent.ArtifactStart -> streamHandler.onArtifactStart(event.title)
            is ChatEvent.ArtifactContent -> streamHandler.onArtifactContent(event.content)
            is ChatEvent.ArtifactEnd -> streamHandler.onArtifactEnd(event.id)
            is ChatEvent.PermissionRequest -> {
                streamHandler.onPermissionRequired(event)
                requestPermissionDecision(event)
            }
            is ChatEvent.Tokens, is ChatEvent.Done -> { /* no-op */ }
            ChatEvent.Stopped -> streamHandler.onStopped()
            is ChatEvent.Error -> streamHandler.onError(event.message)
            is ChatEvent.Unknown -> streamHandler.onUnknown(event.type)
        }
    }

    private fun isTerminalEvent(event: ChatEvent): Boolean =
        event is ChatEvent.Done || event is ChatEvent.Error || event == ChatEvent.Stopped

    private fun finishPromptAndContinue() {
        finishPrompt()
        // 如果思考指示器未被替换（即 AI 没有输出任何内容），直接移除它
        if (!streamHandler.hasOutput) {
            messageView.removeThinkingIndicator()
        }
        val next = if (queuedPrompts.isEmpty()) null else queuedPrompts.removeFirst()
        if (next == null) { renderQueueState(); return }
        runtime?.removeQueuedPrompt(next.id)
        renderQueueState()
        addSystemMessage("Sending queued message...")
        startPrompt(next.prompt, next.message, next.contextNames)
    }

    private fun finishPrompt() {
        if (!generating) return
        generating = false
        messageView.finishAssistantTurn()
        inputPanel.setGenerating(false)
        inputPanel.focusInput()
        renderQueueState()
    }

    private fun copyLastAssistantResponse() {
        if (streamHandler.assistantText.isBlank()) return
        CopyPasteManager.getInstance().setContents(StringSelection(streamHandler.assistantText))
        addSystemMessage("Copied last response.")
    }

    private fun applyLastCodeBlock() {
        val code = extractLastCodeBlock(streamHandler.assistantText)
        if (code.isNullOrBlank()) { Messages.showWarningDialog(project, "No code block found in the last AtomCode response.", "AtomCode"); return }
        val editor = FileEditorManager.getInstance(project).selectedTextEditor
        if (editor == null) { Messages.showWarningDialog(project, "Open an editor file before applying code.", "AtomCode"); return }
        val document = editor.document
        val selection = editor.selectionModel
        val start = if (selection.hasSelection()) selection.selectionStart else editor.caretModel.offset
        val end = if (selection.hasSelection()) selection.selectionEnd else editor.caretModel.offset
        val before = document.text; val after = before.replaceRange(start, end, code)
        val contentFactory = DiffContentFactory.getInstance()
        val request = SimpleDiffRequest("AtomCode Apply Code Preview", contentFactory.create(before), contentFactory.create(after), "Current editor", "After AtomCode")
        DiffManager.getInstance().showDiff(project, request)
        val choice = Messages.showYesNoDialog(project, "Apply the previewed AtomCode code block to the active editor?", "AtomCode", Messages.getQuestionIcon())
        if (choice != Messages.YES) { addSystemMessage("Apply Code cancelled after preview."); return }
        WriteCommandAction.runWriteCommandAction(project, "Apply AtomCode Code", null, Runnable {
            if (selection.hasSelection()) { document.replaceString(selection.selectionStart, selection.selectionEnd, code); selection.removeSelection() }
            else { document.insertString(editor.caretModel.offset, code) }
        })
        addSystemMessage("Applied the last code block to the active editor.")
    }

    private fun renderQueueState() {
        inputPanel.setQueuedPrompts(
            queuedPrompts.map { queued ->
                QueuedPromptView(
                    id = queued.id,
                    text = queued.prompt,
                    contextSummary = queued.contextNames,
                )
            },
        ) { item ->
            queuedPrompts.removeAll { it.id == item.id }
            runtime?.removeQueuedPrompt(item.id)
            renderQueueState()
        }
        rebuildContext()
    }

    private fun requestPermissionDecision(event: ChatEvent.PermissionRequest) {
        // 非破坏性工具自动允许，避免模态对话框阻塞 EDT 导致 daemon 流中断
        // 破坏性操作（bash、write、edit）在 UI 中异步确认
        val isDestructive = event.toolName in setOf("bash", "execute_command", "write_to_file", "replace_in_file", "delete_files")
        if (!isDestructive) {
            addSystemMessage("[Permission] auto-allowed: ${event.toolName}")
            service.respondToPermission(event.sessionId, "allow", event.toolName)
            return
        }

        // 破坏性操作：在 UI 中展示确认信息，通过 daemon 异步响应
        addSystemMessage("[Permission required] ${event.toolName}: ${event.reason}")
        SwingUtilities.invokeLater {
            val args = event.arguments.take(1200)
            val message = buildString {
                appendLine("AtomCode wants to run a tool."); appendLine()
                appendLine("Tool: ${event.toolName}")
                if (event.reason.isNotBlank()) appendLine("Reason: ${event.reason}")
                if (args.isNotBlank()) { appendLine(); appendLine(args) }
            }
            val choice = Messages.showDialog(
                this, message, "AtomCode Tool Permission",
                arrayOf("Allow Once", "Deny", "Always Allow"), 0, Messages.getWarningIcon()
            )
            val decision = when (choice) { 0 -> "allow"; 2 -> "allow_persist"; else -> "deny" }
            addSystemMessage("[Permission] $decision")
            service.respondToPermission(event.sessionId, decision, event.toolName).whenComplete { ok, error ->
                SwingUtilities.invokeLater {
                    if (error != null) addErrorMessage("Permission error: ${error.cause?.message ?: error.message ?: "failed"}")
                    else if (ok != true) addErrorMessage("no pending permission for this session")
                }
            }
        }
    }

    private fun renderConnectionState(state: ConnectionState) {
        header.updateConnectionState(state)
    }

    private fun persistRuntimeSession() {
        val runtime = runtime ?: return
        SessionWorkspace.getInstance(project).updateRuntimeSession(runtime)
    }

    private fun installInputKeyBindings() {
        inputPanel.installKeyBindings(settings.state.sendWithCtrlEnter)
    }

    private fun applyChatSettings() {
        val size = settings.state.chatFontSize
        font = font.deriveFont(size.toFloat())
    }

    // ── Message helpers ──

    private fun addSystemMessage(text: String) {
        messageView.addSystemMessage(text)
    }

    private fun addErrorMessage(text: String) {
        messageView.addError(text)
    }

    // ── Context management ──

    private fun clearPendingContext() {
        pendingContext.clear()
        runtime?.clearContext()
        rebuildContext()
    }

    private fun rebuildContext() {
        inputPanel.setContextItems(pendingContext.toList())
    }

    private fun buildPromptWithContext(prompt: String, context: List<ChatContextItem>): String {
        if (context.isEmpty()) return prompt
        return buildString {
            appendLine("The user has attached the following file(s)/selection(s) for context. The content is provided inline below - DO NOT use read_file to re-read them.")
            appendLine()
            context.forEach { item ->
                val location = if (item.startLine != null && item.endLine != null) " (lines ${item.startLine}-${item.endLine})" else ""
                appendLine("File: ${item.displayName}$location"); appendLine("```${item.language}"); appendLine(item.content); appendLine("```"); appendLine()
            }
            append("User question: "); append(prompt)
        }
    }

    private fun buildAutomaticContext(existingContext: List<ChatContextItem>): List<ChatContextItem> {
        val level = settings.state.contextLevel
        if (level == AtomCodeContextLevel.Minimal) return emptyList()
        val result = mutableListOf<ChatContextItem>()
        if (level == AtomCodeContextLevel.ProjectContext) {
            result += ChatContextItem(path = project.basePath.orEmpty(), displayName = "Project context", language = "text", content = buildString {
                appendLine("Project: ${project.name}"); project.basePath?.let { appendLine("Base path: $it") }
                currentSession?.id?.let { appendLine("AtomCode session: $it") }
            }.trimEnd(), selection = null, startLine = null, endLine = null)
        }
        val editor = FileEditorManager.getInstance(project).selectedTextEditor ?: return result
        val virtualFile = FileDocumentManager.getInstance().getFile(editor.document) ?: return result
        if (existingContext.any { it.path == virtualFile.path } || result.any { it.path == virtualFile.path }) return result
        val path = virtualFile.path
        when (SensitivePathClassifier.classify(path)) {
            PathSensitivity.Block, PathSensitivity.StrongConfirm -> { addSystemMessage("Skipped automatic context for sensitive file ${virtualFile.name}."); return result }
            PathSensitivity.Warn, PathSensitivity.Normal -> Unit
        }
        if (settings.state.autoSaveBeforeRead) {
            com.intellij.openapi.application.WriteIntentReadAction.run { FileDocumentManager.getInstance().saveAllDocuments() }
        }
        val content = editor.document.text
        if (content.isBlank()) return result
        if (content.length > MAX_ATTACHED_FILE_CHARS) { addSystemMessage("Skipped automatic context for ${virtualFile.name}; file is too large."); return result }
        val relative = project.basePath?.let { base -> if (path.startsWith(base)) path.removePrefix(base).trimStart('/', '\\') else path } ?: path
        val displayName = if (settings.state.sendRelativePathWithSelection) relative else path
        result += ChatContextItem(path = path, displayName = displayName, language = virtualFile.extension ?: "text", content = content, selection = null, startLine = null, endLine = null)
        return result
    }

    // ── File attachment ──

    private fun chooseFilesForContext() {
        val descriptor = FileChooserDescriptor(true, false, false, false, false, true).withTitle("Attach Files to AtomCode")
        val projectDir = project.basePath?.let { LocalFileSystem.getInstance().refreshAndFindFileByPath(it) }
        val files = FileChooser.chooseFiles(descriptor, project, projectDir)
        if (files.isEmpty()) return
        files.forEach(::attachVirtualFile)
    }

    private fun attachVirtualFile(file: VirtualFile) {
        val path = file.path
        when (SensitivePathClassifier.classify(path)) {
            PathSensitivity.Block -> { Messages.showWarningDialog(project, "AtomCode will not attach this sensitive file.", "AtomCode"); return }
            PathSensitivity.StrongConfirm -> {
                val choice = Messages.showYesNoDialog(project, "This file may contain sensitive information. Attach it to the next AtomCode message?", "AtomCode", Messages.getWarningIcon())
                if (choice != Messages.YES) return
            }
            PathSensitivity.Warn, PathSensitivity.Normal -> Unit
        }
        if (settings.state.autoSaveBeforeRead) {
            com.intellij.openapi.application.WriteIntentReadAction.run { FileDocumentManager.getInstance().saveAllDocuments() }; file.refresh(false, false)
        }
        val content = try { String(file.contentsToByteArray(), Charsets.UTF_8) } catch (error: Exception) { Messages.showWarningDialog(project, "Could not read ${file.name}: ${error.message}", "AtomCode"); return }
        if (content.isBlank()) return
        if (content.length > MAX_ATTACHED_FILE_CHARS) { Messages.showWarningDialog(project, "This file is too large to attach. Select a smaller file or attach a selection.", "AtomCode"); return }
        val relative = project.basePath?.let { base -> if (path.startsWith(base)) path.removePrefix(base).trimStart('/', '\\') else path } ?: path
        addContext(ChatContextItem(path = path, displayName = relative, language = file.extension ?: "text", content = content, selection = null, startLine = null, endLine = null))
        addSystemMessage("Attached $relative.")
    }

    // ── Slash commands ──

    private fun handleLocalInputCommand(prompt: String): Boolean {
        val command = prompt.split(Regex("\\s+"), limit = 2).firstOrNull()?.lowercase() ?: return false
        return when (command) {
            "/login" -> { addSystemMessage("Opening AtomGit sign-in in your browser..."); login(); true }
            "/codingplan" -> { addSystemMessage("Running CodingPlan setup..."); runSetup(); true }
            else -> false
        }
    }

    // ── Gear menu ──

    internal fun showGearMenu() {
        val menu = JPopupMenu()
        menu.add(JMenuItem("🔌 Connect / Start").apply { addActionListener { connect() } })
        menu.add(JSeparator())
        val providerMenu = JMenu("Provider ▸")
        providerMenu.add(JMenuItem("Create Provider...").apply { addActionListener { showCreateProviderDialog() } })
        providerMenu.add(JMenuItem("Edit Provider...").apply { addActionListener { showEditProviderDialog() } })
        providerMenu.add(JMenuItem("Delete Provider...").apply { addActionListener { deleteSelectedProvider() } })
        providerMenu.add(JSeparator())
        providerMenu.add(JMenuItem("Thinking Settings...").apply { addActionListener { showThinkingDialog() } })
        menu.add(providerMenu); menu.add(JSeparator())
        menu.add(JMenuItem("🔑 Login").apply { addActionListener { login() } })
        menu.add(JMenuItem("🚀 CodingPlan Setup").apply { addActionListener { runSetup() } })
        menu.add(JSeparator())
        menu.add(JMenuItem("➕ New Chat Tab").apply { addActionListener { openAtomCodeChatTab(project, newTab = true) } })
        menu.add(JMenuItem("✖ Close Current Tab").apply { addActionListener { closeCurrentChatTab(project) } })
        menu.add(JSeparator())
        menu.add(JMenuItem("📋 Session History...").apply { addActionListener { showSessionHistory() } })
        menu.add(JMenuItem("✏️ Rename Session").apply { addActionListener { renameSelectedSession() } })
        menu.add(JMenuItem("🗑 Delete Session").apply { addActionListener { deleteSelectedSession() } })
        menu.add(JMenuItem("🔄 Refresh Sessions").apply { addActionListener { refreshSessionList() } })
        menu.add(JSeparator())
        menu.add(JMenuItem("💬 Slash Commands...").apply { addActionListener { showCommandMenu() } })
        menu.add(JSeparator())
        menu.add(JMenuItem("📋 Copy Last Response").apply { addActionListener { copyLastAssistantResponse() } })
        menu.add(JMenuItem("📝 Apply Last Code Block").apply { addActionListener { applyLastCodeBlock() } })
        menu.add(JSeparator())
        menu.add(JMenuItem("📂 Open Changes").apply { addActionListener { openProjectChanges() } })
        menu.add(JMenuItem("🩺 Diagnostics").apply { addActionListener { showDiagnostics() } })
        menu.add(JMenuItem("⚙ Settings...").apply { addActionListener { project.openAtomCodeSettings() } })
        val pointer = java.awt.MouseInfo.getPointerInfo().location; SwingUtilities.convertPointFromScreen(pointer, this); menu.show(this, pointer.x, pointer.y)
    }

    private fun showCommandMenu() {
        val menu = JPopupMenu()
        val items = listOf(
            SlashCommand("/login", "Sign in with AtomGit"), SlashCommand("/codingplan", "Sync CodingPlan models"),
            SlashCommand("/explain", "Explain code"), SlashCommand("/fix", "Fix issues"),
            SlashCommand("/test", "Write tests"), SlashCommand("/refactor", "Refactor code"),
            SlashCommand("/docs", "Add documentation"), SlashCommand("/review", "Review code"),
            SlashCommand("/optimize", "Optimize performance"),
        )
        items.forEach { command ->
            menu.add(JMenuItem("${command.name} - ${command.description}").apply {
                addActionListener { inputPanel.setInputText("${command.name} "); inputPanel.focusInput() }
            })
        }
        val pointer = java.awt.MouseInfo.getPointerInfo().location; SwingUtilities.convertPointFromScreen(pointer, this); menu.show(this, pointer.x, pointer.y)
    }

    private fun showModelPickerPopup() {
        val menu = JPopupMenu()
        setupSnapshot?.models?.forEach { model ->
            menu.add(JMenuItem("${model.model} (${model.provider})").apply {
                if (model.isDefault) font = font.deriveFont(java.awt.Font.BOLD)
                addActionListener { setDefaultModel(model) }
            })
        }
        if (menu.subElements.isNotEmpty()) {
            val pointer = java.awt.MouseInfo.getPointerInfo().location; SwingUtilities.convertPointFromScreen(pointer, this); menu.show(this, pointer.x, pointer.y)
        }
    }

    // ── Utilities ──

    private fun slashPromptTemplate(prompt: String): String? =
        com.atomcode.jetbrains.ui.slashPromptTemplate(prompt)

    private fun extractLastCodeBlock(text: String): String? =
        com.atomcode.jetbrains.ui.extractLastCodeBlock(text)
}

private data class SlashCommand(val name: String, val description: String)
private data class QueuedPrompt(val id: String, val prompt: String, val message: String, val contextNames: List<String>)

data class ChatContextItem(
    val path: String, val displayName: String, val language: String,
    val content: String, val selection: String?, val startLine: Int?, val endLine: Int?,
)

private fun ChatContextItem.toContextItemState(): ContextItemState =
    ContextItemState(
        id = "$path:${startLine ?: 0}:${endLine ?: 0}:${selection?.hashCode() ?: 0}",
        path = path,
        displayName = displayName,
        language = language,
        selectionStartLine = startLine,
        selectionEndLine = endLine,
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
    val matches = Regex("""```[^\n`]*\n([\s\S]*?)```""").findAll(text).toList()
    return matches.lastOrNull()?.groupValues?.getOrNull(1)?.trimEnd()
}
