package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.ChatStreamListener
import com.atomcode.jetbrains.daemon.ConnectionState
import com.atomcode.jetbrains.daemon.ApprovalMode
import com.atomcode.jetbrains.daemon.CreateProviderRequest
import com.atomcode.jetbrains.daemon.ImageInput
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
import com.atomcode.jetbrains.session.SessionWorkspace
import com.atomcode.jetbrains.settings.AtomCodeContextLevel
import com.atomcode.jetbrains.settings.AtomCodeSettingsState
import com.atomcode.jetbrains.ui.header.HeaderPanel
import com.atomcode.jetbrains.ui.input.InputPanel
import com.atomcode.jetbrains.ui.input.QueuedPromptView
import com.atomcode.jetbrains.ui.message.JBCefMessageView
import com.atomcode.jetbrains.ui.message.MessageAttachmentView
import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.openapi.application.ApplicationManager
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
import com.intellij.ide.ClipboardSynchronizer
import com.intellij.ide.BrowserUtil
import com.intellij.openapi.ide.CopyPasteManager
import com.intellij.ui.mac.foundation.Foundation
import com.intellij.ui.mac.foundation.ID
import com.intellij.ui.JBColor
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.Component
import java.awt.Dialog
import java.awt.Dimension
import java.awt.Font
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Image
import java.awt.Insets
import java.awt.Toolkit
import java.awt.datatransfer.DataFlavor
import java.awt.datatransfer.StringSelection
import java.awt.datatransfer.Transferable
import java.io.ByteArrayInputStream
import java.awt.image.BufferedImage
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.net.URI
import java.nio.ByteBuffer
import java.beans.PropertyChangeEvent
import java.time.Duration
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Base64
import java.util.Locale
import java.util.UUID
import javax.imageio.ImageIO
import javax.swing.BorderFactory
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JDialog
import javax.swing.ImageIcon
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
import javax.swing.ListCellRenderer
import javax.swing.ListSelectionModel
import javax.swing.JOptionPane
import javax.swing.SwingUtilities
import javax.swing.Timer
import javax.swing.UIManager
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

private const val MAX_ATTACHED_FILE_CHARS = 120_000
private const val CHAT_REQUEST_BODY_LIMIT_BYTES = 32 * 1024 * 1024
private const val CHAT_REQUEST_BODY_RESERVED_BYTES = 1 * 1024 * 1024
private const val MAX_ATTACHED_IMAGE_BYTES =
    (CHAT_REQUEST_BODY_LIMIT_BYTES - CHAT_REQUEST_BODY_RESERVED_BYTES) * 3 / 4
private const val MAX_ATTACHED_IMAGE_MB = MAX_ATTACHED_IMAGE_BYTES / 1024 / 1024
private const val MIN_CHAT_PANEL_WIDTH = 360
private const val MIN_CHAT_PANEL_HEIGHT = 300
private const val CLIPBOARD_IMAGE_PATH_PREFIX = "clipboard-image://"
private const val ATOMCODE_DOCS_ZH_URL = "https://atomcode.atomgit.com/docs/zh/index.html"
private const val ATOMCODE_DOCS_EN_URL = "https://atomcode.atomgit.com/docs/en/index.html"

private val SESSION_HISTORY_TODAY_TIME_FORMAT: DateTimeFormatter = DateTimeFormatter.ofPattern("HH:mm")
private val SESSION_HISTORY_YEAR_TIME_FORMAT: DateTimeFormatter = DateTimeFormatter.ofPattern("MM-dd HH:mm")
private val SESSION_HISTORY_FULL_TIME_FORMAT: DateTimeFormatter = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")

private fun sessionUpdatedInstant(updatedAt: Long): Instant? {
    if (updatedAt <= 0L) return null
    val epochMillis = if (updatedAt < 10_000_000_000L) updatedAt * 1000L else updatedAt
    return runCatching { Instant.ofEpochMilli(epochMillis) }.getOrNull()
}

private fun formatSessionUpdatedAt(updatedAt: Long): String {
    val instant = sessionUpdatedInstant(updatedAt) ?: return "未知"
    val now = Instant.now()
    val minutes = Duration.between(instant, now).toMinutes()
    if (minutes in 0..1) return "刚刚"
    if (minutes in 2..59) return "${minutes} 分钟前"

    val zone = ZoneId.systemDefault()
    val dateTime = instant.atZone(zone)
    val today = LocalDate.now(zone)
    return when {
        dateTime.toLocalDate() == today -> "今天 ${SESSION_HISTORY_TODAY_TIME_FORMAT.format(dateTime)}"
        dateTime.year == today.year -> SESSION_HISTORY_YEAR_TIME_FORMAT.format(dateTime)
        else -> SESSION_HISTORY_FULL_TIME_FORMAT.format(dateTime)
    }
}

private fun formatSessionUpdatedAtFull(updatedAt: Long): String {
    val instant = sessionUpdatedInstant(updatedAt) ?: return "未知"
    return SESSION_HISTORY_FULL_TIME_FORMAT.format(instant.atZone(ZoneId.systemDefault()))
}

private class SessionHistoryCellRenderer : JPanel(BorderLayout(12, 0)), ListCellRenderer<SessionMeta> {
    private val title = JLabel()
    private val updated = JLabel()

    init {
        isOpaque = true
        border = BorderFactory.createEmptyBorder(4, 8, 4, 8)
        title.isOpaque = false
        updated.isOpaque = false
        add(title, BorderLayout.CENTER)
        add(updated, BorderLayout.EAST)
    }

    override fun getListCellRendererComponent(
        list: JList<out SessionMeta>,
        value: SessionMeta?,
        index: Int,
        isSelected: Boolean,
        cellHasFocus: Boolean,
    ): Component {
        background = if (isSelected) list.selectionBackground else list.background
        val foreground = if (isSelected) list.selectionForeground else list.foreground
        title.foreground = foreground
        updated.foreground = if (isSelected) foreground else UIUtil.getContextHelpForeground()
        title.font = list.font.deriveFont(Font.PLAIN)
        updated.font = list.font.deriveFont(Font.PLAIN, list.font.size2D - 1f)

        if (value == null) {
            title.text = ""
            updated.text = ""
            toolTipText = null
        } else {
            title.text = "${value.displayName} (${value.messageCount})"
            updated.text = formatSessionUpdatedAt(value.updatedAt)
            toolTipText = "最后更新：${formatSessionUpdatedAtFull(value.updatedAt)}"
        }
        return this
    }
}

class AtomCodeChatPanel(
    private val project: Project,
    private val runtime: ChatRuntime? = null,
) : JPanel(BorderLayout()), Disposable {
    private val service = AtomCodeProjectService.getInstance(project)
    private val settings = AtomCodeSettingsState.getInstance()

    // ── UI components ──
    private val header = HeaderPanel()
    private val messageView = JBCefMessageView { action -> handleWelcomeAction(action) }
    /** 持久化失败警告条文本标签，由 [persistenceWarningBar] CENTER 区持有；直接引用避免位置查找。 */
    private val persistenceWarningText = JLabel().apply {
        foreground = JBColor(0x6B5200, 0xF0D890)
    }
    /** 持久化失败警告条（时间线外展示，符合 daemon 契约）。默认隐藏。 */
    private val persistenceWarningBar = JPanel(BorderLayout(8, 0)).apply {
        isVisible = false
        border = BorderFactory.createEmptyBorder(4, 8, 4, 8)
        background = JBColor(0xFFF3E2, 0x5A4A1F)
        add(
            JLabel("⚠").apply { border = BorderFactory.createEmptyBorder(0, 0, 0, 4) },
            BorderLayout.WEST,
        )
        add(persistenceWarningText, BorderLayout.CENTER)
        add(
            JButton("×").apply {
                isOpaque = false
                border = BorderFactory.createEmptyBorder(0, 4, 0, 0)
                isContentAreaFilled = false
                toolTipText = "Dismiss"
                addActionListener { dismissPersistenceWarning() }
            },
            BorderLayout.EAST,
        )
    }
    private val inputPanel = InputPanel(
        onSend = { text -> handleSend(text) },
        onStop = { stopCurrentGeneration() },
        onAttach = { chooseFilesForContext() },
        onSlashCommand = { showCommandMenu() },
        onClearContext = { clearPendingContext() },
        onRemoveContext = { item -> removePendingAttachment(item) },
        onModelSelect = { showModelPickerPopup() },
        onApprovalModeSelect = { mode -> setApprovalMode(mode) },
        onPasteFromClipboard = { transferable -> pasteClipboardImage(transferable) },
    )

    // ── Data state ──
    private val modelPicker = JComboBox<ModelInfo>().apply {
        prototypeDisplayValue = ModelInfo("provider", "model-name", "openai", false)
    }
    private val sessionPicker = JComboBox<SessionMeta>().apply {
        prototypeDisplayValue = SessionMeta("00000000", "Recent conversation title", "", 0L, 99)
    }
    private var loadingSessions = false
    private var loadingModels = false
    private var generating = false
    private var generationSequence = 0L
    private var activeGenerationId: Long? = null
    private var setupSnapshot: SetupSnapshot? = null
    private var currentSession: SessionRefView? = null
    private var welcomeLanguage: String = defaultWelcomeLanguage()
    private var loggedIn = false
    private val setupRefreshTimer = Timer(2_000) {
        if (isShowing && !disposed && service.connectionState is ConnectionState.Ready) {
            refreshSetupSnapshot(silent = true)
        }
    }.apply {
        isRepeats = true
        initialDelay = 2_000
    }
    private val streamHandler = StreamEventHandler(messageView)
    private val pendingContext = mutableListOf<ChatContextItem>()
    private val pendingImages = mutableListOf<PendingImageAttachment>()
    private val queuedPrompts = ArrayDeque<QueuedPrompt>()
    private var disposed = false
    private val connectionListener = java.beans.PropertyChangeListener { event: PropertyChangeEvent ->
        if (disposed) return@PropertyChangeListener
        SwingUtilities.invokeLater {
            if (!disposed) {
                renderConnectionState(event.newValue as ConnectionState)
                if (event.newValue is ConnectionState.Ready) {
                    inputPanel.setApprovalMode(service.approvalMode)
                    inputPanel.setApprovalModeEnabled(!service.approvalModePending)
                    refreshSetupSnapshot()
                    refreshSessionList()
                }
            }
        }
    }

    init {
        minimumSize = Dimension(MIN_CHAT_PANEL_WIDTH, MIN_CHAT_PANEL_HEIGHT)
        applyTheme()

        // ── Assemble 3-zone layout ──
        // 消息区与持久化警告条包在 messageZone 中：警告条固定在滚动区外
        // （消息时间线下方、输入框上方），符合 daemon 契约的时间线外展示。
        val messageZone = JPanel(BorderLayout()).apply {
            background = UIUtil.getPanelBackground()
            add(messageView, BorderLayout.CENTER)
            add(persistenceWarningBar, BorderLayout.SOUTH)
        }
        add(header, BorderLayout.NORTH)
        add(messageZone, BorderLayout.CENTER)
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
        setupRefreshTimer.start()
        renderConnectionState(service.connectionState)
        applyChatSettings()
        showWelcomePage()

        refreshAfterConnect()
    }

    override fun updateUI() {
        super.updateUI()
        applyTheme()
    }

    private fun applyTheme() {
        background = UIUtil.getPanelBackground()
        border = BorderFactory.createMatteBorder(
            1,
            1,
            1,
            1,
            UIManager.getColor("Component.borderColor") ?: JBColor.border(),
        )
    }

    /** 展示持久化失败警告（时间线外警告条，不进入消息流）。 */
    private fun showPersistenceWarning(message: String) {
        if (disposed) return
        persistenceWarningText.text = message
        persistenceWarningBar.isVisible = true
    }

    /** 隐藏持久化失败警告条（手动关闭 / 新回合 / 清空会话）。 */
    private fun dismissPersistenceWarning() {
        if (disposed) return
        persistenceWarningBar.isVisible = false
    }

    override fun dispose() {
        if (disposed) return
        disposed = true
        queuedPrompts.clear()
        pendingContext.clear()
        pendingImages.clear()
        service.removeConnectionListener(connectionListener)
        setupRefreshTimer.stop()
        messageView.dispose()
    }

    fun focusInput() {
        inputPanel.focusInput()
    }

    fun showWelcomePage() {
        messageView.showWelcomePage(welcomeLanguage, loggedIn)
    }

    fun submitPrompt(prompt: String) {
        inputPanel.setInputText(prompt)
        handleSend(prompt)
    }

    fun composePrompt(prompt: String, context: ChatContextItem? = null) {
        context?.let(::addContext)
        inputPanel.setInputText(prompt)
        focusInput()
    }

    private fun handleWelcomeAction(action: String) {
        when {
            action == "settings" -> showGearMenu()
            action == "login" -> login()
            action == "docs" -> BrowserUtil.browse(currentDocsUrl())
            action == "review" -> composePrompt("/review ")
            action.startsWith("prompt:") -> composePrompt(action.removePrefix("prompt:"))
            action.startsWith("language:") -> {
                welcomeLanguage = normalizeWelcomeLanguage(action.removePrefix("language:"))
                showWelcomePage()
            }
        }
    }

    private fun currentDocsUrl(): String =
        if (welcomeLanguage == "zh") ATOMCODE_DOCS_ZH_URL else ATOMCODE_DOCS_EN_URL

    private fun normalizeWelcomeLanguage(language: String): String =
        if (language.equals("zh", ignoreCase = true)) "zh" else "en"

    private fun defaultWelcomeLanguage(): String =
        if (Locale.getDefault().language.equals("zh", ignoreCase = true)) "zh" else "en"

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
        service.ensureConnected().thenAccept { state ->
            if (state is ConnectionState.Ready) {
                SwingUtilities.invokeLater {
                    refreshSetupSnapshot()
                    refreshSessionList()
                }
            }
        }
    }

    private fun refreshSetupSnapshot(silent: Boolean = false) {
        service.loadSetupSnapshot().whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    if (!silent) {
                        addErrorMessage(error.cause?.message ?: error.message ?: "failed to load setup")
                    }
                    return@invokeLater
                }
                if (snapshot != setupSnapshot) renderSetupSnapshot(snapshot)
            }
        }
    }

    private fun renderSetupSnapshot(snapshot: SetupSnapshot) {
        setupSnapshot = snapshot
        loggedIn = snapshot.auth?.let { it.loggedIn && !it.expired } == true

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
        if (currentSession == null && !generating) {
            showWelcomePage()
        }
    }

    private fun login() {
        service.loginWithBrowser { message ->
            SwingUtilities.invokeLater {
                header.updateLoginStatus(message)
            }
        }.whenComplete { snapshot, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    header.updateLoginStatus(
                        "Login failed: ${error.cause?.message ?: error.message ?: "failed"}",
                        failed = true,
                    )
                    refreshSetupSnapshot()
                    return@invokeLater
                }
                renderSetupSnapshot(snapshot)
                header.updateConnectionState(service.connectionState)
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

    private fun setApprovalMode(mode: ApprovalMode) {
        inputPanel.setApprovalModeEnabled(false)
        service.setApprovalMode(mode).whenComplete { applied, error ->
            SwingUtilities.invokeLater {
                inputPanel.setApprovalModeEnabled(true)
                if (error != null) {
                    addErrorMessage(error.cause?.message ?: error.message ?: "failed to set approval mode")
                    inputPanel.setApprovalMode(service.approvalMode)
                    startNextQueuedPromptIfReady()
                    return@invokeLater
                }
                inputPanel.setApprovalMode(applied)
                startNextQueuedPromptIfReady()
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
                persistRuntimeSession()
                messageView.clear()
                dismissPersistenceWarning()
                addSystemMessage("Started new session ${session.name.ifBlank { session.id.take(8) }}.")
                refreshSessionList()
                inputPanel.focusInput()
            }
        }
    }

    internal fun showSessionHistory() {
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
        val list = JList(model).apply {
            selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
            visibleRowCount = 14
            fixedCellHeight = 34
            cellRenderer = SessionHistoryCellRenderer()
        }
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
                    updateCurrentSessionTitle(selected.id, nextName)
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
                    refill(sessions); addSystemMessage("Deleted ${selected.size} session(s).")
                    if (currentSession == null) showWelcomePage()
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
                replaceSessions(sessions, selected.id)
                updateCurrentSessionTitle(selected.id, nextName)
                addSystemMessage("Session renamed to $nextName.")
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
                addSystemMessage("Session deleted.")
                if (currentSession == null) showWelcomePage()
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

    private fun refreshSessionList(updateCurrentTabTitle: Boolean = false) {
        service.refreshSessions().whenComplete { sessions, error ->
            SwingUtilities.invokeLater {
                if (error != null) { addErrorMessage(error.cause?.message ?: error.message ?: "failed to load sessions"); return@invokeLater }
                replaceSessions(sessions, currentSession?.id)
                if (updateCurrentTabTitle) {
                    val activeSessionId = currentSession?.id
                    sessions.firstOrNull { it.id == activeSessionId }?.let {
                        updateCurrentSessionTitle(it.id, it.displayName)
                    }
                }
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
                updateAtomCodeChatTabTitle(project, this@AtomCodeChatPanel, detail.name.ifBlank { detail.id.take(8) })
                persistRuntimeSession()
                replaceSelectedSession(detail.id); renderSession(detail); inputPanel.focusInput()
            }
        }
    }

    private fun renderSession(detail: SessionDetail) {
        messageView.clear()
        dismissPersistenceWarning()
        var assistantGroupOpen = false
        detail.messages.forEach { message ->
            val role = message.role.lowercase()
            when (role) {
                "user" -> {
                    if (message.synthetic || isInternalHistoryUserMessage(message.content)) return@forEach
                    val restored = decodeHistoryUserMessage(message.content)
                    messageView.addUserMessage(restored.text, restored.contextSummary)
                    assistantGroupOpen = false
                }
                "assistant" -> {
                    if (isInternalHistoryAssistantMessage(message)) return@forEach
                    if (!shouldRenderHistoryAssistantText(message) && message.toolCalls.isEmpty()) {
                        return@forEach
                    }
                    if (!shouldRenderHistoryAssistantText(message)) {
                        if (!assistantGroupOpen) {
                            messageView.beginAssistantTurn()
                        }
                        renderHistoryAssistantToolCalls(message)
                        messageView.finishAssistantTurn()
                        assistantGroupOpen = true
                        return@forEach
                    }
                    if (!assistantGroupOpen) {
                        messageView.beginAssistantTurn()
                    }
                    messageView.addAssistantMessage(message.content)
                    renderHistoryAssistantToolCalls(message)
                    messageView.finishAssistantTurn()
                    assistantGroupOpen = true
                }
                "tool" -> {
                    renderHistoryToolMessage(message.content)
                    assistantGroupOpen = true
                }
                "system" -> Unit
                else -> {
                    addSystemMessage(message.content)
                    assistantGroupOpen = false
                }
            }
        }
    }

    private fun rerenderFinishedSessionFromHistory(session: SessionRefView) {
        Timer(150) {
            (it.source as? Timer)?.stop()
            loadFinishedSessionFromHistory(session)
        }.apply {
            isRepeats = false
            start()
        }
    }

    private fun loadFinishedSessionFromHistory(session: SessionRefView) {
        val meta = SessionMeta(
            id = session.id,
            name = session.name,
            projectHash = session.projectHash,
            updatedAt = 0L,
            messageCount = 0,
        )

        service.loadSessionDetail(meta).whenComplete { detail, error ->
            SwingUtilities.invokeLater {
                if (error != null) return@invokeLater
                if (generating || activeGenerationId != null) return@invokeLater
                if (currentSession?.id != session.id) return@invokeLater

                currentSession = SessionRefView(detail.id, detail.name, detail.projectHash, detail.workingDir)
                updateAtomCodeChatTabTitle(project, this@AtomCodeChatPanel, detail.name.ifBlank { detail.id.take(8) })
                persistRuntimeSession()
                replaceSelectedSession(detail.id)
                renderSession(detail)
                streamHandler.replayLastTurnSummary()
                inputPanel.focusInput()
            }
        }
    }

    private fun renderHistoryToolMessage(content: String) {
        val detail = content.trim()
        if (detail.isBlank()) return
        messageView.addToolCall("tool", "done", detail, "历史工具结果")
    }

    private fun renderHistoryAssistantToolCalls(message: MessageInfo) {
        message.toolCalls.forEach { toolCall ->
            val detail = toolCall.arguments.trim()
            messageView.addToolCall(
                toolCall.name.ifBlank { "tool" },
                "done",
                detail.takeIf { it.isNotBlank() },
                "历史工具调用",
            )
        }
    }

    // ── Send / Chat streaming ──

    private fun handleSend(text: String): Boolean {
        val prompt = text.trim()
        if (prompt.isEmpty()) return false
        if (service.approvalModePending) return false
        if (handleLocalInputCommand(prompt)) {
            inputPanel.clearInput()
            return true
        }
        val transformedPrompt = slashPromptTemplate(prompt) ?: prompt
        val pendingContextForSend = pendingContext.toList()
        val pendingImagesForSend = pendingImages.toList()
        val contextForSend = pendingContextForSend + buildAutomaticContext(pendingContextForSend)
        val message = buildPromptWithContext(transformedPrompt, contextForSend)
        val contextNames = contextForSend.map { it.displayName } + pendingImagesForSend.map { it.displayName }
        val attachments = contextForSend.map { MessageAttachmentView(displayName = it.displayName, path = it.path) } +
            pendingImagesForSend.map { it.toMessageAttachmentView() }
        val images = pendingImagesForSend.map { it.toImageInput() }
        val approvalModeForSend = service.confirmedApprovalMode

        if (generating) {
            val queued = QueuedPrompt(
                UUID.randomUUID().toString(),
                transformedPrompt,
                message,
                contextNames,
                attachments,
                images,
                approvalModeForSend,
            )
            queuedPrompts += queued
            if (pendingContextForSend.isNotEmpty() || pendingImagesForSend.isNotEmpty()) clearPendingContext()
            inputPanel.clearInput()
            renderQueueState()
            return true
        }
        if (pendingContextForSend.isNotEmpty() || pendingImagesForSend.isNotEmpty()) clearPendingContext()
        startPrompt(transformedPrompt, message, contextNames, attachments, images, approvalModeForSend)
        return true
    }

    private fun startPrompt(
        prompt: String,
        message: String,
        contextNames: List<String>,
        attachments: List<MessageAttachmentView>,
        images: List<ImageInput>,
        approvalMode: ApprovalMode,
    ) {
        val generationId = ++generationSequence
        activeGenerationId = generationId
        // Add user message + immediate thinking feedback
        renderQueueState()
        messageView.addUserMessage(prompt, contextNames, attachments)
        messageView.beginAssistantTurn()
        messageView.addThinkingIndicator()
        inputPanel.clearInput()
        dismissPersistenceWarning()
        generating = true
        inputPanel.setGenerating(true)
        streamHandler.reset()

        val provider = (modelPicker.selectedItem as? ModelInfo)?.provider
        service.sendPrompt(message, currentSession, object : ChatStreamListener {
            override fun onEvent(event: ChatEvent) {
                SwingUtilities.invokeLater {
                    if (activeGenerationId != generationId) return@invokeLater
                    renderChatEvent(event)
                    if (isTerminalEvent(event)) finishPromptAndContinue(generationId)
                }
            }
            override fun onComplete() {
                SwingUtilities.invokeLater {
                    if (activeGenerationId != generationId) return@invokeLater
                    streamHandler.onComplete()
                    finishPromptAndContinue(generationId)
                }
            }
            override fun onError(message: String) {
                SwingUtilities.invokeLater {
                    if (activeGenerationId == generationId) streamHandler.onError(message)
                }
            }
        }, onSessionReady = { session ->
            SwingUtilities.invokeLater {
                setCurrentSessionReference(session)
                replaceSelectedSession(session.id)
                persistRuntimeSession()
            }
        }, provider = provider, images = images, approvalMode = approvalMode).whenComplete { session, error ->
            SwingUtilities.invokeLater {
                if (error != null) {
                    finishPromptAndContinue(generationId)
                } else if (session != null) {
                    setCurrentSessionReference(session)
                    replaceSelectedSession(session.id)
                    persistRuntimeSession()
                    rerenderFinishedSessionFromHistory(session)
                }
            }
        }
    }

    private fun renderChatEvent(event: ChatEvent) {
        when (event) {
            is ChatEvent.RuntimeInfo -> {
                inputPanel.setModelName(event.model)
                refreshSetupSnapshot(silent = true)
            }
            is ChatEvent.Text -> streamHandler.onText(event.content)
            is ChatEvent.Reasoning -> streamHandler.onReasoning(event.content)
            is ChatEvent.ToolBatch -> streamHandler.onToolBatch()
            is ChatEvent.ToolStart -> streamHandler.onToolStart(event.name, event.arguments)
            is ChatEvent.ToolOutput -> streamHandler.onToolOutput(event.chunk)
            is ChatEvent.ToolProgress -> streamHandler.onToolProgress(event.progress)
            is ChatEvent.ToolResult -> streamHandler.onToolResult(event.name, event.output, event.success, event.durationMs)
            is ChatEvent.ArtifactStart -> streamHandler.onArtifactStart(
                event.id,
                event.artifactType,
                event.language,
                event.title,
            )
            is ChatEvent.ArtifactContent -> streamHandler.onArtifactContent(event.id, event.content)
            is ChatEvent.ArtifactEnd -> streamHandler.onArtifactEnd(event.id)
            is ChatEvent.PermissionRequest -> {
                streamHandler.onPermissionRequired(event)
                requestPermissionDecision(event)
            }
            is ChatEvent.Tokens -> { /* no-op */ }
            is ChatEvent.Warning -> streamHandler.onWarning(event.message)
            is ChatEvent.PersistenceWarning -> showPersistenceWarning(event.message)
            is ChatEvent.Done -> {
                streamHandler.onDone(event.tokens, event.toolCalls)
                refreshSessionList(updateCurrentTabTitle = true)
            }
            ChatEvent.Stopped -> streamHandler.onStopped()
            is ChatEvent.Error -> streamHandler.onError(event.message)
            is ChatEvent.Unknown -> streamHandler.onUnknown(event.type)
        }
    }

    private fun isTerminalEvent(event: ChatEvent): Boolean =
        event is ChatEvent.Done || event is ChatEvent.Error || event == ChatEvent.Stopped

    private fun finishPromptAndContinue(generationId: Long) {
        if (!generating || activeGenerationId != generationId) return

        messageView.finishAssistantTurn()
        val next = nextQueuedPromptIfModeReady()
        if (next == null) {
            finishPrompt(generationId, assistantAlreadyFinished = true)
            return
        }

        // Keep the composer in its generating state while handing off to the queued
        // prompt. Clearing it for one event-loop turn causes the visible flash.
        startQueuedPrompt(next)
    }

    private fun startNextQueuedPromptIfReady() {
        if (generating) return
        val next = nextQueuedPromptIfModeReady() ?: return
        startQueuedPrompt(next)
    }

    private fun nextQueuedPromptIfModeReady(): QueuedPrompt? {
        if (service.approvalModePending || queuedPrompts.isEmpty()) return null
        return queuedPrompts.removeFirst()
    }

    private fun startQueuedPrompt(next: QueuedPrompt) {
        activeGenerationId = null
        startPrompt(
            next.prompt,
            next.message,
            next.contextNames,
            next.attachments,
            next.images,
            next.approvalMode,
        )
    }

    private fun finishPrompt(
        expectedGenerationId: Long? = activeGenerationId,
        assistantAlreadyFinished: Boolean = false,
    ) {
        if (!generating) return
        if (expectedGenerationId != null && activeGenerationId != expectedGenerationId) return
        activeGenerationId = null
        generating = false
        if (!assistantAlreadyFinished) messageView.finishAssistantTurn()
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
        val session = currentSession ?: return
        SessionWorkspace.getInstance(project).updateTabSession(runtime.tabId, session)
    }

    private fun updateCurrentSessionTitle(sessionId: String, title: String) {
        val session = currentSession?.takeIf { it.id == sessionId } ?: return
        val normalizedTitle = title.trim().ifBlank { session.id.take(8) }
        currentSession = session.copy(name = normalizedTitle)
        updateAtomCodeChatTabTitle(project, this, normalizedTitle)
        persistRuntimeSession()
    }

    private fun setCurrentSessionReference(session: SessionRefView) {
        val existing = currentSession
        currentSession = if (existing?.id == session.id) {
            session.copy(name = existing.name)
        } else {
            session
        }
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
        pendingImages.clear()
        rebuildContext()
    }

    private fun rebuildContext() {
        inputPanel.setContextItems(pendingContext.toList() + pendingImages.map { it.toContextItem() })
    }

    private fun removePendingAttachment(item: ChatContextItem) {
        val removedContext = pendingContext.remove(item)
        val removedImage = pendingImages.removeAll { it.path == item.path }
        if (removedContext || removedImage) rebuildContext()
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
            ApplicationManager.getApplication().runWriteAction {
                FileDocumentManager.getInstance().saveAllDocuments()
            }
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
            ApplicationManager.getApplication().runWriteAction {
                FileDocumentManager.getInstance().saveAllDocuments()
                file.refresh(false, false)
            }
        }
        val mediaType = imageMediaType(file)
        if (mediaType != null) {
            attachImageFile(file, mediaType)
            return
        }
        val content = try { String(file.contentsToByteArray(), Charsets.UTF_8) } catch (error: Exception) { Messages.showWarningDialog(project, "Could not read ${file.name}: ${error.message}", "AtomCode"); return }
        if (content.isBlank()) return
        if (content.length > MAX_ATTACHED_FILE_CHARS) { Messages.showWarningDialog(project, "This file is too large to attach. Select a smaller file or attach a selection.", "AtomCode"); return }
        val relative = project.basePath?.let { base -> if (path.startsWith(base)) path.removePrefix(base).trimStart('/', '\\') else path } ?: path
        addContext(ChatContextItem(path = path, displayName = relative, language = file.extension ?: "text", content = content, selection = null, startLine = null, endLine = null))
    }

    private fun attachImageFile(file: VirtualFile, mediaType: String) {
        val bytes = try {
            file.contentsToByteArray()
        } catch (error: Exception) {
            Messages.showWarningDialog(project, "Could not read ${file.name}: ${error.message}", "AtomCode")
            return
        }
        if (bytes.isEmpty()) return
        val attachedBytes = pendingImages.sumOf { it.byteSize }
        if (attachedBytes + bytes.size > MAX_ATTACHED_IMAGE_BYTES) {
            Messages.showWarningDialog(
                project,
                "Attached images are too large. Select image(s) totaling under $MAX_ATTACHED_IMAGE_MB MB.",
                "AtomCode",
            )
            return
        }
        val relative = project.basePath?.let { base ->
            if (file.path.startsWith(base)) file.path.removePrefix(base).trimStart('/', '\\') else file.path
        } ?: file.path
        if (pendingImages.none { it.path == file.path }) {
            pendingImages += PendingImageAttachment(
                path = file.path,
                displayName = relative,
                mediaType = mediaType,
                byteSize = bytes.size,
                data = Base64.getEncoder().encodeToString(bytes),
            )
        }
        rebuildContext()
        focusInput()
    }

    private fun pasteClipboardImage(transferable: Transferable?): Boolean {
        if (attachFilesFromTransferable(transferable)) return true

        val bytes = clipboardImageBytes(transferable)
        if (bytes == null) {
            val diagnosticTransferable = transferable?.takeIf { it.hasImageLikeFlavor() }
                ?: firstImageLikeClipboardTransferable()
            val macTypes = macPasteboardTypeNames()
            if (diagnosticTransferable != null || macTypes.any(::isImagePasteboardType)) {
                addSystemMessage(clipboardImageDiagnostic(diagnosticTransferable, macTypes))
                return true
            }
            return false
        }

        attachClipboardImage(bytes)
        return true
    }

    private fun attachFilesFromTransferable(transferable: Transferable?): Boolean {
        val files = filesFromTransferable(transferable)
        if (files.isEmpty()) return false

        var attached = false
        files.forEach { file ->
            val virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByPath(file.absolutePath)
            if (virtualFile != null && !virtualFile.isDirectory) {
                attachVirtualFile(virtualFile)
                attached = true
            }
        }
        return attached
    }

    private fun clipboardImageBytes(transferable: Transferable?): ByteArray? {
        imageBytesFromTransferable(transferable)?.let { return it }

        val copyPasteManager = CopyPasteManager.getInstance()
        imageBytesFromTransferable(copyPasteManager.contents)?.let { return it }
        copyPasteManager.allContents.firstNotNullOfOrNull(::imageBytesFromTransferable)?.let { return it }

        imageBytesFromTransferable(ClipboardSynchronizer.getInstance().contents)?.let { return it }

        val systemTransferable = try {
            Toolkit.getDefaultToolkit().systemClipboard.getContents(null)
        } catch (_: Exception) {
            null
        }
        imageBytesFromTransferable(systemTransferable)?.let { return it }

        return macPasteboardImageBytes()
    }

    private fun Transferable.hasImageLikeFlavor(): Boolean =
        transferDataFlavors.any { it.looksLikeImageFlavor() }

    private fun firstImageLikeClipboardTransferable(): Transferable? {
        val copyPasteManager = CopyPasteManager.getInstance()
        sequenceOf(copyPasteManager.contents, ClipboardSynchronizer.getInstance().contents)
            .filterNotNull()
            .firstOrNull { it.hasImageLikeFlavor() }
            ?.let { return it }

        copyPasteManager.allContents
            .firstOrNull { it.hasImageLikeFlavor() }
            ?.let { return it }

        val systemTransferable = try {
            Toolkit.getDefaultToolkit().systemClipboard.getContents(null)
        } catch (_: Exception) {
            null
        }
        return systemTransferable?.takeIf { it.hasImageLikeFlavor() }
    }

    private fun clipboardImageDiagnostic(transferable: Transferable?, macTypes: List<String>): String {
        val javaFlavors = transferable?.transferDataFlavors
            .orEmpty()
            .joinToString(separator = "\n") { flavor ->
                "- ${flavor.mimeType}; class=${flavor.representationClass.name}; name=${flavor.humanPresentableName}"
            }
            .ifBlank { "- <none>" }
            .take(4000)
        val macTypeText = macTypes
            .joinToString(separator = "\n") { "- $it" }
            .ifBlank { "- <none or unavailable>" }
            .take(2000)
        return "检测到图片剪贴板，但未能解析为附件，已阻止 IDE 默认粘贴写入项目文件。Java clipboard flavors:\n$javaFlavors\nmacOS pasteboard types:\n$macTypeText"
    }

    private fun macPasteboardTypeNames(): List<String> {
        if (!System.getProperty("os.name").contains("mac", ignoreCase = true)) return emptyList()
        return try {
            val pasteboard = Foundation.invoke("NSPasteboard", "generalPasteboard")
            if (Foundation.isNil(pasteboard)) return emptyList()
            val types = Foundation.safeInvoke(pasteboard, "types")
            if (Foundation.isNil(types)) return emptyList()
            Foundation.NSArray(types).getList().mapNotNull { id ->
                runCatching { Foundation.toStringViaUTF8(id) }.getOrNull()
            }
        } catch (_: Throwable) {
            emptyList()
        }
    }

    private fun isImagePasteboardType(type: String): Boolean {
        val normalized = type.lowercase(Locale.ROOT)
        return normalized.contains("image") ||
            normalized.contains("png") ||
            normalized.contains("tiff") ||
            normalized.contains("jpeg") ||
            normalized.contains("jpg") ||
            normalized.contains("pict")
    }

    private fun macPasteboardImageBytes(): ByteArray? {
        if (!System.getProperty("os.name").contains("mac", ignoreCase = true)) return null
        return try {
            val pasteboard = Foundation.invoke("NSPasteboard", "generalPasteboard")
            if (Foundation.isNil(pasteboard)) return null

            pasteboardImageObjects(pasteboard)?.let { return it }

            val types = listOf(
                "public.png",
                "public.tiff",
                "public.jpeg",
                "com.apple.tiff",
                "com.apple.pict",
                "Apple TIFF pasteboard type",
                "NeXT TIFF v4.0 pasteboard type",
                "NSPasteboardTypePNG",
                "NSPasteboardTypeTIFF",
                "NSPasteboardTypeJPEG",
            )
            types.firstNotNullOfOrNull { type ->
                pasteboardDataForType(pasteboard, type)?.let(::imageBytesToPngBytes)
            }
        } catch (_: Throwable) {
            null
        }
    }

    private fun pasteboardImageObjects(pasteboard: ID): ByteArray? {
        val imageClass = Foundation.getObjcClass("NSImage")
        if (Foundation.isNil(imageClass)) return null
        val classes = Foundation.fillArray(arrayOf(imageClass))
        val images = Foundation.safeInvoke(pasteboard, "readObjectsForClasses:options:", classes, ID.NIL)
        if (Foundation.isNil(images)) return null

        val list = Foundation.NSArray(images)
        for (index in 0 until list.count()) {
            val image = list.at(index)
            if (Foundation.isNil(image)) continue
            val data = Foundation.safeInvoke(image, "TIFFRepresentation")
            if (Foundation.isNil(data)) continue
            val bytes = Foundation.NSData(data).bytes()
            imageBytesToPngBytes(bytes)?.let { return it }
        }
        return null
    }

    private fun pasteboardDataForType(pasteboard: ID, type: String): ByteArray? {
        val data = Foundation.safeInvoke(pasteboard, "dataForType:", Foundation.nsString(type))
        if (Foundation.isNil(data)) return null
        return Foundation.NSData(data).bytes().takeIf { it.isNotEmpty() }
    }

    private fun imageBytesFromTransferable(transferable: Transferable?): ByteArray? {
        if (transferable == null) return null

        if (transferable.isDataFlavorSupported(DataFlavor.imageFlavor)) {
            val image = try {
                transferable.getTransferData(DataFlavor.imageFlavor) as? Image
            } catch (_: Exception) {
                null
            }
            if (image != null) return imageToPngBytes(image)
        }

        if (transferable.isDataFlavorSupported(DataFlavor.javaFileListFlavor)) {
            val files = try {
                @Suppress("UNCHECKED_CAST")
                transferable.getTransferData(DataFlavor.javaFileListFlavor) as? List<File>
            } catch (_: Exception) {
                null
            }
            files.orEmpty().firstNotNullOfOrNull(::imageFileToPngBytes)?.let { return it }
        }

        transferable.transferDataFlavors.forEach { flavor ->
            if (!flavor.looksLikeImageFlavor()) return@forEach
            val data = try {
                transferable.getTransferData(flavor)
            } catch (_: Exception) {
                null
            }
            dataToPngBytes(data)?.let { return it }
        }

        return null
    }

    private fun filesFromTransferable(transferable: Transferable?): List<File> {
        if (transferable == null) return emptyList()

        if (transferable.isDataFlavorSupported(DataFlavor.javaFileListFlavor)) {
            val files = try {
                @Suppress("UNCHECKED_CAST")
                transferable.getTransferData(DataFlavor.javaFileListFlavor) as? List<File>
            } catch (_: Exception) {
                null
            }
            if (!files.isNullOrEmpty()) return files
        }

        return transferable.transferDataFlavors
            .filter { it.isUriListFlavor() }
            .firstNotNullOfOrNull { flavor ->
                val data = try {
                    transferable.getTransferData(flavor)
                } catch (_: Exception) {
                    null
                }
                uriListToFiles(data as? String).takeIf { it.isNotEmpty() }
            }
            .orEmpty()
    }

    private fun DataFlavor.isUriListFlavor(): Boolean =
        mimeType.lowercase(Locale.ROOT).contains("text/uri-list")

    private fun uriListToFiles(uriList: String?): List<File> {
        if (uriList.isNullOrBlank()) return emptyList()
        return uriList
            .lineSequence()
            .map { it.trim() }
            .filter { it.isNotEmpty() && !it.startsWith("#") }
            .mapNotNull { value ->
                runCatching {
                    val uri = URI(value)
                    if (uri.scheme.equals("file", ignoreCase = true)) File(uri) else null
                }.getOrNull()
            }
            .toList()
    }

    private fun DataFlavor.looksLikeImageFlavor(): Boolean {
        if (Image::class.java.isAssignableFrom(representationClass)) return true
        val mime = mimeType.lowercase(Locale.ROOT)
        val name = humanPresentableName.lowercase(Locale.ROOT)
        return primaryType.equals("image", ignoreCase = true) ||
            mime.contains("image/") ||
            mime.contains("public.png") ||
            mime.contains("public.tiff") ||
            mime.contains("public.jpeg") ||
            name.contains("png") ||
            name.contains("tiff") ||
            name.contains("jpeg") ||
            name.contains("jpg")
    }

    private fun dataToPngBytes(data: Any?): ByteArray? =
        when (data) {
            is Image -> imageToPngBytes(data)
            is ByteArray -> imageBytesToPngBytes(data)
            is ByteBuffer -> imageBytesToPngBytes(data.toByteArray())
            is InputStream -> data.use { imageBytesToPngBytes(it.readBytes()) }
            is File -> imageFileToPngBytes(data)
            is List<*> -> data.filterIsInstance<File>().firstNotNullOfOrNull(::imageFileToPngBytes)
            else -> null
        }

    private fun ByteBuffer.toByteArray(): ByteArray {
        val duplicate = duplicate()
        val bytes = ByteArray(duplicate.remaining())
        duplicate.get(bytes)
        return bytes
    }

    private fun imageFileToPngBytes(file: File): ByteArray? {
        if (!file.isFile) return null
        return try {
            imageBytesToPngBytes(file.readBytes())
        } catch (_: Exception) {
            null
        }
    }

    private fun imageBytesToPngBytes(bytes: ByteArray): ByteArray? {
        if (bytes.isEmpty()) return null
        val image = try {
            ImageIO.read(ByteArrayInputStream(bytes))
        } catch (_: Exception) {
            null
        } ?: return null
        return bufferedImageToPngBytes(image)
    }

    private fun attachClipboardImage(bytes: ByteArray) {
        if (bytes.isEmpty()) return
        val attachedBytes = pendingImages.sumOf { it.byteSize }
        if (attachedBytes + bytes.size > MAX_ATTACHED_IMAGE_BYTES) {
            Messages.showWarningDialog(
                project,
                "Attached images are too large. Paste image(s) totaling under $MAX_ATTACHED_IMAGE_MB MB.",
                "AtomCode",
            )
            return
        }

        val index = pendingImages.count { it.path.startsWith(CLIPBOARD_IMAGE_PATH_PREFIX) } + 1
        pendingImages += PendingImageAttachment(
            path = "$CLIPBOARD_IMAGE_PATH_PREFIX${UUID.randomUUID()}.png",
            displayName = "Clipboard image $index.png",
            mediaType = "image/png",
            byteSize = bytes.size,
            data = Base64.getEncoder().encodeToString(bytes),
        )
        rebuildContext()
        focusInput()
    }

    private fun imageToPngBytes(image: Image): ByteArray? {
        val icon = ImageIcon(image)
        val width = icon.iconWidth
        val height = icon.iconHeight
        if (width <= 0 || height <= 0) return null

        val buffered = BufferedImage(width, height, BufferedImage.TYPE_INT_ARGB)
        val graphics = buffered.createGraphics()
        try {
            graphics.drawImage(icon.image, 0, 0, null)
        } finally {
            graphics.dispose()
        }

        return bufferedImageToPngBytes(buffered)
    }

    private fun bufferedImageToPngBytes(buffered: BufferedImage): ByteArray? =
        ByteArrayOutputStream().use { out ->
            if (!ImageIO.write(buffered, "png", out)) return null
            out.toByteArray()
        }

    private fun imageMediaType(file: VirtualFile): String? =
        when (file.extension?.lowercase(Locale.ROOT)) {
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "gif" -> "image/gif"
            "webp" -> "image/webp"
            else -> null
        }

    // ── Slash commands ──

    private fun handleLocalInputCommand(prompt: String): Boolean {
        val command = prompt.split(Regex("\\s+"), limit = 2).firstOrNull()?.lowercase() ?: return false
        return when (command) {
            "/login" -> { login(); true }
            else -> false
        }
    }

    // ── Gear menu ──

    internal fun showGearMenu() {
        val labels = gearMenuLabels()
        val menu = JPopupMenu()
        menu.add(JMenuItem(labels.connectStart).apply { addActionListener { connect() } })
        menu.add(JSeparator())
        val providerMenu = JMenu(labels.provider)
        providerMenu.add(JMenuItem(labels.createProvider).apply { addActionListener { showCreateProviderDialog() } })
        providerMenu.add(JMenuItem(labels.editProvider).apply { addActionListener { showEditProviderDialog() } })
        providerMenu.add(JMenuItem(labels.deleteProvider).apply { addActionListener { deleteSelectedProvider() } })
        providerMenu.add(JSeparator())
        providerMenu.add(JMenuItem(labels.thinkingSettings).apply { addActionListener { showThinkingDialog() } })
        menu.add(providerMenu); menu.add(JSeparator())
        menu.add(JMenuItem(labels.login).apply { addActionListener { login() } })
        menu.add(JMenuItem(labels.codingPlanSetup).apply { addActionListener { runSetup() } })
        menu.add(JSeparator())
        menu.add(JMenuItem(labels.sessionHistory).apply { addActionListener { showSessionHistory() } })
        menu.add(JMenuItem(labels.renameSession).apply { addActionListener { renameSelectedSession() } })
        menu.add(JMenuItem(labels.deleteSession).apply { addActionListener { deleteSelectedSession() } })
        menu.add(JMenuItem(labels.refreshSessions).apply { addActionListener { refreshSessionList() } })
        menu.add(JSeparator())
        menu.add(JMenuItem(labels.openChanges).apply { addActionListener { openProjectChanges() } })
        menu.add(JMenuItem(labels.diagnostics).apply { addActionListener { showDiagnostics() } })
        menu.add(JMenuItem(labels.settings).apply { addActionListener { project.openAtomCodeSettings() } })
        val pointer = java.awt.MouseInfo.getPointerInfo().location; SwingUtilities.convertPointFromScreen(pointer, this); menu.show(this, pointer.x, pointer.y)
    }

    private fun showCommandMenu() {
        val menu = JPopupMenu()
        val items = listOf(
            SlashCommand("/login", "登录 AtomGit"),
            SlashCommand("/review", "审查代码"),
        )
        items.forEach { command ->
            menu.add(JMenuItem("${command.name} - ${command.description}").apply {
                addActionListener { inputPanel.setInputText("${command.name} "); inputPanel.focusInput() }
            })
        }
        inputPanel.showCommandPopup(menu)
    }

    private fun showModelPickerPopup() {
        val menu = JPopupMenu()
        // 跟随 IDE 主题的选中背景色，退化到固定蓝调保证可读
        val activeBg = UIManager.getColor("List.selectionBackground") ?: JBColor(0x2F5FA8, 0x2F5FA8)
        setupSnapshot?.models?.forEach { model ->
            menu.add(JMenuItem("${model.model} (${model.provider})").apply {
                if (model.isDefault) {
                    font = font.deriveFont(java.awt.Font.BOLD)
                    isOpaque = true
                    background = activeBg
                }
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
private data class QueuedPrompt(
    val id: String,
    val prompt: String,
    val message: String,
    val contextNames: List<String>,
    val attachments: List<MessageAttachmentView>,
    val images: List<ImageInput>,
    val approvalMode: ApprovalMode,
)

private data class PendingImageAttachment(
    val path: String,
    val displayName: String,
    val mediaType: String,
    val byteSize: Int,
    val data: String,
) {
    fun toImageInput(): ImageInput = ImageInput(mediaType = mediaType, data = data)

    fun toMessageAttachmentView(): MessageAttachmentView =
        MessageAttachmentView(
            displayName = displayName,
            path = path,
            imageMediaType = mediaType,
            imageData = data,
        )

    fun toContextItem(): ChatContextItem =
        ChatContextItem(
            path = path,
            displayName = displayName,
            language = mediaType.substringAfter('/').uppercase(Locale.ROOT),
            content = "",
            selection = null,
            startLine = null,
            endLine = null,
            imageMediaType = mediaType,
            imageData = data,
        )
}

data class ChatContextItem(
    val path: String, val displayName: String, val language: String,
    val content: String, val selection: String?, val startLine: Int?, val endLine: Int?,
    val imageMediaType: String? = null, val imageData: String? = null,
)

internal data class HistoryUserMessage(
    val text: String,
    val contextSummary: List<String>,
)

/** Restores the visible prompt from the context-enriched message persisted by the daemon. */
internal fun decodeHistoryUserMessage(content: String): HistoryUserMessage {
    val questionMarker = "\nUser question: "
    val questionIndex = content.lastIndexOf(questionMarker)
    if (!content.startsWith("The user has attached the following file(s)/selection(s) for context.") || questionIndex < 0) {
        return HistoryUserMessage(content, emptyList())
    }

    val contextPrefix = content.substring(0, questionIndex)
    val contextNames = HISTORY_CONTEXT_FILE_PATTERN.findAll(contextPrefix)
        .map { match -> match.groupValues[1].trim() }
        .toList()
    val question = content.substring(questionIndex + questionMarker.length)
    return HistoryUserMessage(question, contextNames)
}

internal fun isInternalHistoryUserMessage(content: String): Boolean {
    val trimmed = content.trim()
    return INTERNAL_HISTORY_USER_PREFIXES.any { trimmed.startsWith(it) }
}

internal fun isInternalHistoryAssistantMessage(message: MessageInfo): Boolean {
    return message.role.equals("assistant", ignoreCase = true) &&
        message.internalOrigin == "verify_cadence" &&
        message.toolCalls.isEmpty()
}

internal fun shouldRenderHistoryAssistantText(message: MessageInfo): Boolean {
    return message.content.isNotBlank()
}

private val INTERNAL_HISTORY_USER_PREFIXES = listOf(
    "<system-reminder>",
    "You made code edits but have not verified them.",
    "Output limit hit — your last response was cut off",
    "Output limit hit. If the task is already complete",
    "[PLAN MODE",
    "[Context was compressed",
    "[Additional context from user]:",
    "[SYNTAX CHECK:",
    "[DEV SERVER ERROR",
    "[Auto-read from error:",
    "[Images returned by the tool calls above",
)

private val HISTORY_CONTEXT_FILE_PATTERN =
    Regex("(?m)^File: (.+?)(?: \\(lines \\d+-\\d+\\))?\\r?\\n```")

internal fun slashPromptTemplate(prompt: String): String? {
    val parts = prompt.split(Regex("\\s+"), limit = 2)
    val command = parts.firstOrNull()?.lowercase() ?: return null
    val suffix = parts.getOrNull(1)?.trim().orEmpty()
    val template = when (command) {
        "/review" -> "请审查这段代码，重点关注潜在问题、改进建议和最佳实践。"
        else -> return null
    }
    return if (suffix.isBlank()) template else "$template\n\n$suffix"
}

internal fun extractLastCodeBlock(text: String): String? {
    val matches = Regex("""(?m)^[ \t]*(`{3,})[^\n`]*\n([\s\S]*?)^[ \t]*\1[ \t]*$""")
        .findAll(text)
        .toList()
    return matches.lastOrNull()?.groupValues?.getOrNull(2)?.trimEnd()
}
