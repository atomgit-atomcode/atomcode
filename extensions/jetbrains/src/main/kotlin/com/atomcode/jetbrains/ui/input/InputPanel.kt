package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ApprovalMode
import com.atomcode.jetbrains.ui.ChatContextItem
import com.intellij.ide.PasteProvider
import com.intellij.openapi.actionSystem.DataContext
import com.intellij.openapi.actionSystem.DataProvider
import com.intellij.openapi.actionSystem.PlatformDataKeys
import com.intellij.ui.JBColor
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.Component
import java.awt.Cursor
import java.awt.GraphicsEnvironment
import java.awt.Dimension
import java.awt.Graphics
import java.awt.Graphics2D
import java.awt.Insets
import java.awt.KeyboardFocusManager
import java.awt.RenderingHints
import java.awt.Toolkit
import java.awt.Image
import java.awt.datatransfer.DataFlavor
import java.awt.datatransfer.Transferable
import java.awt.event.InputEvent
import java.awt.event.KeyEvent
import java.awt.KeyEventDispatcher
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import javax.swing.BoxLayout
import javax.swing.AbstractAction
import javax.swing.BorderFactory
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JMenuItem
import javax.swing.MenuElement
import javax.swing.JPanel
import javax.swing.JPopupMenu
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.KeyStroke
import javax.swing.Action
import javax.swing.TransferHandler
import javax.swing.SwingUtilities
import javax.swing.UIManager
import javax.swing.text.DefaultEditorKit
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

/**
 * 输入区域容器：ContextChips + 输入行 + 底部工具栏。Claude Code 风格。
 */
class InputPanel(
    private val onSend: (String) -> Boolean,
    private val onStop: () -> Unit,
    private val onAttach: () -> Unit,
    private val onSlashCommand: () -> Unit,
    private val onClearContext: () -> Unit,
    private val onRemoveContext: (ChatContextItem) -> Unit,
    private val onModelSelect: () -> Unit,
    private val onApprovalModeSelect: (ApprovalMode) -> Unit = {},
    private val onEffortSelect: (String?) -> Unit = {},
    private val onPasteFromClipboard: (Transferable?) -> Boolean,
) : JPanel(BorderLayout()), DataProvider, PasteProvider {

    private var slashTriggerConsumed = false

    /// `/` 选择器：向上弹出、限高可滚动；焦点始终留在输入框，所以打字就是过滤。
    /// 输入框外框，选择器左沿与它对齐（而不是文字区那一点内缩）。
    private lateinit var composerPanel: JPanel

    private val commandPopup = SlashCommandPopup(
        anchor = { inputArea },
        onInsert = { name ->
            setInputText("$name ")
            focusInput()
        },
        horizontalAnchor = { if (::composerPanel.isInitialized) composerPanel else inputArea },
        // 收起后复位：焦点移开导致收起，再回来继续输入时仍要重新触发匹配。
        onHide = { slashTriggerConsumed = false },
    )
    private var initialized = false
    private val toolButtons = mutableListOf<JButton>()
    private val compactToolButtons = mutableSetOf<JButton>()
    private var defaultPasteAction: Action? = null
    private var pasteDispatcherRegistered = false
    private val menuShortcutMask = Toolkit.getDefaultToolkit().menuShortcutKeyMaskEx
    private val pasteKeyDispatcher = KeyEventDispatcher { event ->
        if (event.id != KeyEvent.KEY_PRESSED || event.keyCode != KeyEvent.VK_V) return@KeyEventDispatcher false
        if (event.modifiersEx and menuShortcutMask != menuShortcutMask) return@KeyEventDispatcher false
        val focusOwner = KeyboardFocusManager.getCurrentKeyboardFocusManager().focusOwner ?: return@KeyEventDispatcher false
        if (!SwingUtilities.isDescendingFrom(focusOwner, inputArea)) return@KeyEventDispatcher false
        onPasteFromClipboard(null)
    }

    private val inputArea = PasteAwareTextArea().apply {
        rows = 3
        lineWrap = true
        wrapStyleWord = true
        border = BorderFactory.createEmptyBorder(6, 10, 6, 10)
        font = font.deriveFont(font.size2D)
    }

    private val sendButton = makeToolButton("↑ 发送") { fireSend() }.apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
        preferredSize = Dimension(preferredSize.width.coerceAtLeast(92), 30)
    }

    private val stopButton = JButton("⏹ 停止").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
        preferredSize = Dimension(92, 30)
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        isContentAreaFilled = true
        isBorderPainted = true
        isFocusPainted = false
        isOpaque = true
        margin = Insets(0, 0, 0, 0)
        addActionListener { onStop() }
    }

    private val contextChips = ContextChipsPanel(onClear = onClearContext)
    private val queueChips = PromptQueuePanel()

    private val tokenLabel = JLabel("").apply {
        font = font.deriveFont(font.size2D - 2f)
    }

    private val modelLabel = JLabel("GPT-4o ▾").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 2f)
        isOpaque = false
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        addMouseListener(object : MouseAdapter() {
            override fun mouseClicked(e: MouseEvent) = onModelSelect()
        })
    }

    private val approvalModeLabel = JLabel("${ApprovalMode.Build} ▾").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 2f)
        isOpaque = false
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        toolTipText = "模式"
        accessibleContext.accessibleName = "Approval mode"
        border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        addMouseListener(object : MouseAdapter() {
            override fun mouseClicked(e: MouseEvent) {
                if (isEnabled) showApprovalModeMenu()
            }
        })
    }

    /// Sits between the mode and the model, the way the VS Code client orders them.
    /// Hidden whenever the selected model exposes no effort levels.
    private val effortLabel = JLabel("${EffortChoices.DEFAULT_LABEL} ▾").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 2f)
        isOpaque = false
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        toolTipText = "推理强度"
        accessibleContext.accessibleName = "Reasoning effort"
        border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        isVisible = false
        addMouseListener(object : MouseAdapter() {
            override fun mouseClicked(e: MouseEvent) {
                if (isEnabled && isVisible) showEffortMenu()
            }
        })
    }

    private var effortChoices: List<EffortChoice> = emptyList()

    /// The composer toolbar; kept only so tests can drive [layoutToolbar].
    private var toolbarRef: JPanel? = null

    private val actionCards = CardLayout()
    private val actionPanel = JPanel(actionCards).apply {
        isOpaque = false
        add(sendButton, SEND_CARD)
        add(stopButton, STOP_CARD)
        preferredSize = Dimension(92, 30)
    }

    init {
        isOpaque = true
        installTransferHandler()

        inputArea.document.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(event: DocumentEvent) = handleInputChanged()
            override fun removeUpdate(event: DocumentEvent) = handleInputChanged()
            override fun changedUpdate(event: DocumentEvent) = handleInputChanged()
        })

        val inputScroll = JScrollPane(inputArea).apply {
            border = BorderFactory.createEmptyBorder()
            isOpaque = false
            viewport.isOpaque = false
            preferredSize = Dimension(200, 68)
            minimumSize = Dimension(100, 48)
            horizontalScrollBarPolicy = JScrollPane.HORIZONTAL_SCROLLBAR_NEVER
            verticalScrollBarPolicy = JScrollPane.VERTICAL_SCROLLBAR_NEVER
        }

        val attachButton = makeCompactToolButton("📎", "附件", onAttach)
        val commandButton = makeCompactToolButton("/", "命令") { showSlashCommandsFromButton() }

        // 工具栏与输入框放在同一个 composer 容器内，状态切换时布局保持稳定。
        val toolbar = object : JPanel(null) {
            override fun doLayout() {
                layoutToolbar(
                    this,
                    attachButton,
                    commandButton,
                    tokenLabel,
                    approvalModeLabel,
                    effortLabel,
                    modelLabel,
                    actionPanel,
                )
            }
        }.apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(5, 2, 1, 2)
            preferredSize = Dimension(200, 36)
            minimumSize = Dimension(0, 36)
            add(attachButton)
            add(commandButton)
            add(tokenLabel)
            add(approvalModeLabel)
            add(effortLabel)
            add(modelLabel)
            add(actionPanel)
        }
        toolbarRef = toolbar

        val composer = JPanel(BorderLayout()).apply {
            isOpaque = true
            background = INPUT_BG
            border = BorderFactory.createCompoundBorder(
                BorderFactory.createLineBorder(COMPOSER_BORDER, 1, true),
                BorderFactory.createEmptyBorder(5, 9, 7, 7),
            )
            add(contextChips, BorderLayout.NORTH)
            add(inputScroll, BorderLayout.CENTER)
            add(toolbar, BorderLayout.SOUTH)
        }

        composerPanel = composer

        val composerInset = JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(8, 10, 10, 10)
            add(composer, BorderLayout.CENTER)
        }

        val chips = JPanel().apply {
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            isOpaque = false
            add(queueChips)
        }

        add(chips, BorderLayout.NORTH)
        add(composerInset, BorderLayout.CENTER)
        initialized = true
        applyTheme()
    }

    override fun updateUI() {
        super.updateUI()
        if (initialized) {
            SwingUtilities.invokeLater {
                if (initialized) applyTheme()
            }
        }
    }

    override fun addNotify() {
        super.addNotify()
        if (!pasteDispatcherRegistered) {
            KeyboardFocusManager.getCurrentKeyboardFocusManager().addKeyEventDispatcher(pasteKeyDispatcher)
            pasteDispatcherRegistered = true
        }
    }

    override fun removeNotify() {
        if (pasteDispatcherRegistered) {
            KeyboardFocusManager.getCurrentKeyboardFocusManager().removeKeyEventDispatcher(pasteKeyDispatcher)
            pasteDispatcherRegistered = false
        }
        super.removeNotify()
    }

    override fun getData(dataId: String): Any? =
        if (PlatformDataKeys.PASTE_PROVIDER.`is`(dataId)) this else null

    override fun performPaste(dataContext: DataContext) {
        if (!onPasteFromClipboard(null)) {
            performDefaultPaste()
        }
    }

    override fun isPastePossible(dataContext: DataContext): Boolean = inputArea.isEnabled

    override fun isPasteEnabled(dataContext: DataContext): Boolean = inputArea.isEnabled

    private inner class PasteAwareTextArea : JTextArea(), DataProvider, PasteProvider {
        var placeholderText: String = "Enter 发送 · Shift+Enter 换行"

        override fun paintComponent(graphics: Graphics) {
            super.paintComponent(graphics)
            if (text.isNotEmpty() || placeholderText.isBlank()) return

            val graphics2D = graphics.create() as Graphics2D
            try {
                graphics2D.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, RenderingHints.VALUE_TEXT_ANTIALIAS_ON)
                val baseFont = UIUtil.getLabelFont().deriveFont(font.size2D)
                val insets = insets
                val usableWidth = (width - insets.left - insets.right).coerceAtLeast(0)
                val x = insets.left
                var y = insets.top

                graphics2D.font = baseFont
                val primary = "输入消息"
                val primaryMetrics = graphics2D.fontMetrics
                y += primaryMetrics.ascent
                graphics2D.color = PLACEHOLDER_FG
                graphics2D.drawString(primary, x, y)

                graphics2D.font = baseFont.deriveFont(baseFont.size2D - 1f)
                val secondaryMetrics = graphics2D.fontMetrics
                val secondary = "  $placeholderText"
                val secondaryX = x + primaryMetrics.stringWidth(primary) + 6
                if (secondaryX + secondaryMetrics.stringWidth(secondary) <= insets.left + usableWidth) {
                    graphics2D.color = PLACEHOLDER_SECONDARY_FG
                    graphics2D.drawString(secondary, secondaryX, y)
                }
            } finally {
                graphics2D.dispose()
            }
        }

        override fun paste() {
            if (!onPasteFromClipboard(null)) {
                super.paste()
            }
        }

        override fun getData(dataId: String): Any? =
            if (PlatformDataKeys.PASTE_PROVIDER.`is`(dataId)) this else null

        override fun performPaste(dataContext: DataContext) {
            if (!onPasteFromClipboard(null)) {
                performDefaultPaste()
            }
        }

        override fun isPastePossible(dataContext: DataContext): Boolean = isEnabled

        override fun isPasteEnabled(dataContext: DataContext): Boolean = isEnabled
    }

    private fun installTransferHandler() {
        val defaultTransferHandler = inputArea.transferHandler
        inputArea.transferHandler = object : TransferHandler() {
            override fun canImport(support: TransferSupport): Boolean =
                support.hasAttachableFlavor() || defaultTransferHandler?.canImport(support) == true

            override fun importData(support: TransferSupport): Boolean {
                if (support.hasAttachableFlavor()) {
                    if (onPasteFromClipboard(support.transferable)) return true
                }
                return defaultTransferHandler?.importData(support) == true
            }
        }
    }

    private fun TransferHandler.TransferSupport.hasAttachableFlavor(): Boolean =
        dataFlavors.any { flavor ->
            Image::class.java.isAssignableFrom(flavor.representationClass) ||
                flavor.primaryType.equals("image", ignoreCase = true) ||
                flavor.mimeType.lowercase().contains("image/") ||
                flavor.mimeType.lowercase().contains("public.png") ||
                flavor.mimeType.lowercase().contains("public.tiff") ||
                flavor.mimeType.lowercase().contains("public.jpeg") ||
                flavor == DataFlavor.javaFileListFlavor ||
                flavor.mimeType.lowercase().contains("text/uri-list")
        }

    fun getInputText(): String = inputArea.text

    fun setInputText(text: String) {
        inputArea.text = text
    }

    fun focusInput() {
        inputArea.requestFocusInWindow()
    }

    /// 打开 `/` 选择器。列表由调用方给出（内置命令 + daemon 的技能），过滤与
    /// 滚动都在 [SlashCommandPopup] 里，焦点一直留在输入框。
    internal fun showCommandPopup(commands: List<SlashCommand>) {
        inputArea.requestFocusInWindow()
        commandPopup.show(commands, commandPrefix().orEmpty())
    }

    /// 技能列表异步到达：选择器开着就地更新，别让人重开一次才看到。
    internal fun updateCommandPopup(commands: List<SlashCommand>) {
        commandPopup.updateCommands(commands, commandPrefix().orEmpty())
    }

    fun setGenerating(generating: Boolean) {
        actionCards.show(actionPanel, if (generating) STOP_CARD else SEND_CARD)
    }

    fun setContextItems(items: List<ChatContextItem>) {
        contextChips.setItems(items) { item -> onRemoveContext(item) }
    }

    fun setQueuedPrompts(items: List<QueuedPromptView>, onRemove: (QueuedPromptView) -> Unit) {
        queueChips.setItems(items, onRemove)
    }

    fun setModelName(name: String) {
        modelLabel.text = "$name ▾"
        modelLabel.parent?.revalidate()
    }

    /// Refresh the effort selector for the selected model. An empty list hides it
/// entirely — a model without declared levels has nothing to choose.
    internal fun setEffortChoices(choices: List<EffortChoice>, current: String?) {
        effortChoices = choices
        effortLabel.isVisible = effortChipHasContent()
        effortLabel.text = "${EffortChoices.labelFor(choices, current)} ▾"
        effortLabel.parent?.revalidate()
        effortLabel.parent?.repaint()
    }

    /// The chip only exists when the selected model exposes switchable levels; an
    /// endpoint declaring none must not leave a lone "Default" chip in the toolbar.
    private fun effortChipHasContent(): Boolean = effortChoices.isNotEmpty()

    fun setEffortEnabled(enabled: Boolean) {
        effortLabel.isEnabled = enabled
        effortLabel.foreground = if (enabled) MODEL_FG else DISABLED_FG
        effortLabel.repaint()
    }

    internal fun effortDisplayTextForTest(): String = effortLabel.text

    internal fun effortVisibleForTest(): Boolean = effortLabel.isVisible

    internal fun effortOptionsForTest(): List<String> = effortChoices.map { it.label }

    internal fun selectEffortForTest(value: String?) = onEffortSelect(value)

    /// Test hook: run the toolbar layout as a real layout pass would, so tests can
    /// prove the chip stays hidden after `setEffortChoices` hides it.
    internal fun layoutToolbarForTest(width: Int) {
        val toolbar = toolbarRef ?: return
        toolbar.setSize(width, toolbar.height.coerceAtLeast(36))
        toolbar.doLayout()
    }

    fun setApprovalMode(mode: ApprovalMode) {
        approvalModeLabel.text = "$mode ▾"
        approvalModeLabel.parent?.revalidate()
        approvalModeLabel.repaint()
    }

    fun setApprovalModeEnabled(enabled: Boolean) {
        approvalModeLabel.isEnabled = enabled
        approvalModeLabel.foreground = if (enabled) MODEL_FG else DISABLED_FG
        approvalModeLabel.repaint()
    }

    internal fun approvalModeOptionsForTest(): List<String> =
        ApprovalMode.values().map { it.toString() }

    internal fun approvalModeDisplayTextForTest(): String =
        approvalModeLabel.text

    internal fun selectApprovalModeForTest(mode: ApprovalMode) {
        onApprovalModeSelect(mode)
    }

    fun setTokenCount(current: Int, max: Int) {
        tokenLabel.text = if (max > 0) "$current/$max" else ""
        tokenLabel.parent?.revalidate()
    }

    fun clearInput() {
        inputArea.text = ""
    }

    fun installKeyBindings(sendWithCtrlEnter: Boolean) {
        val enterAction = "atomcode-input-enter"
        val ctrlEnterAction = "atomcode-input-ctrl-enter"
        val shiftEnterAction = "atomcode-input-shift-enter"
        val tabAction = "atomcode-command-tab"
        val pasteAction = "atomcode-input-paste"
        defaultPasteAction = inputArea.actionMap.get(DefaultEditorKit.pasteAction)

        inputArea.inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_V, menuShortcutMask), pasteAction)
        inputArea.inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_V, InputEvent.CTRL_DOWN_MASK), pasteAction)
        inputArea.actionMap.put(DefaultEditorKit.pasteAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!onPasteFromClipboard(null)) {
                    performDefaultPaste(e)
                }
            }
        })
        inputArea.actionMap.put(pasteAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!onPasteFromClipboard(null)) {
                    performDefaultPaste(e)
                }
            }
        })

        inputArea.placeholderText = if (sendWithCtrlEnter) {
            "Ctrl+Enter 发送 · Enter 换行"
        } else {
            "Enter 发送 · Shift+Enter 换行"
        }
        inputArea.repaint()

        inputArea.inputMap.put(KeyStroke.getKeyStroke("UP"), "atomcode-command-up")
        inputArea.actionMap.put("atomcode-command-up", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!commandPopup.move(-1)) {
                    inputArea.transferFocusBackward()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("DOWN"), "atomcode-command-down")
        inputArea.actionMap.put("atomcode-command-down", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!commandPopup.move(1)) {
                    inputArea.transferFocus()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ESCAPE"), "atomcode-command-escape")
        inputArea.actionMap.put("atomcode-command-escape", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                hideCommandPopup()
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("TAB"), tabAction)
        inputArea.actionMap.put(tabAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!commandPopup.activate()) {
                    inputArea.transferFocus()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ENTER"), enterAction)
        inputArea.actionMap.put(enterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (commandPopup.activate()) return
                if (sendWithCtrlEnter) {
                    insertNewline()
                } else {
                    fireSend()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("shift ENTER"), shiftEnterAction)
        inputArea.actionMap.put(shiftEnterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                insertNewline()
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ctrl ENTER"), ctrlEnterAction)
        inputArea.actionMap.put(ctrlEnterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (sendWithCtrlEnter) {
                    fireSend()
                } else {
                    insertNewline()
                }
            }
        })
    }

    private fun insertNewline() {
        inputArea.replaceSelection("\n")
    }

    private fun performDefaultPaste(event: java.awt.event.ActionEvent? = null) {
        val action = defaultPasteAction
        if (action != null) {
            action.actionPerformed(event)
        } else {
            inputArea.paste()
        }
    }

    /// `/` 按钮是开关：选择器开着就清空输入并收起，否则补上 `/` 再打开
    /// （与 vscode 的 handleSlashButton 一致）。
    private fun showSlashCommandsFromButton() {
        if (commandPopup.isVisible) {
            slashTriggerConsumed = false
            commandPopup.hide()
            inputArea.text = ""
            inputArea.requestFocusInWindow()
            return
        }
        if (commandPrefix() == null) {
            slashTriggerConsumed = true
            inputArea.text = "/"
            inputArea.caretPosition = inputArea.document.length
        }
        inputArea.requestFocusInWindow()
        onSlashCommand()
    }

    private fun showApprovalModeMenu() {
        val menu = JPopupMenu()
        ApprovalMode.values().forEach { mode ->
            menu.add(JMenuItem(mode.toString()).apply {
                addActionListener { onApprovalModeSelect(mode) }
            })
        }
        menu.show(approvalModeLabel, 0, -menu.preferredSize.height)
    }

    private fun showEffortMenu() {
        val menu = JPopupMenu()
        effortChoices.forEach { choice ->
            menu.add(JMenuItem(choice.label).apply {
                addActionListener { onEffortSelect(choice.value) }
            })
        }
        menu.show(effortLabel, 0, -menu.preferredSize.height)
    }

    private fun fireSend() {
        commandPopup.hide()
        val text = inputArea.text.trim()
        if (text.isNotEmpty()) {
            if (onSend(text)) {
                inputArea.text = ""
            }
        }
    }

    private fun hideCommandPopup() {
        commandPopup.hide()
        inputArea.requestFocusInWindow()
    }

    private fun handleInputChanged() {
        inputArea.repaint()
        handleSlashTrigger()
    }

    private fun handleSlashTrigger() {
        val prefix = commandPrefix()
        if (prefix == null) {
            slashTriggerConsumed = false
            commandPopup.hide()
            return
        }

        if (commandPopup.isVisible) {
            commandPopup.update(prefix)
            return
        }

        if (slashTriggerConsumed) return

        slashTriggerConsumed = true
        SwingUtilities.invokeLater {
            val currentPrefix = commandPrefix()
            if (currentPrefix != null) {
                onSlashCommand()
                SwingUtilities.invokeLater { commandPopup.update(currentPrefix) }
            }
        }
    }

    private fun commandPrefix(): String? {
        return slashCommandPrefix(inputArea.text)
    }

    private fun layoutToolbar(
        toolbar: JPanel,
        attach: Component,
        command: Component,
        token: Component,
        mode: Component,
        effort: Component,
        model: Component,
        action: Component,
    ) {
        val available = toolbar.width.takeIf { it > 0 } ?: return
        val insets = toolbar.insets
        val left = insets.left
        val right = available - insets.right
        val top = insets.top
        val height = (toolbar.height - insets.top - insets.bottom).coerceAtLeast(0)
        val gap = 8

        listOf(attach, command, token, mode, effort, model, action).forEach { it.isVisible = false }

        val actionSize = action.preferredSize
        val actionWidth = actionSize.width.coerceAtMost((right - left).coerceAtLeast(0))
        val actionX = right - actionWidth
        action.setBounds(actionX, top + ((height - actionSize.height) / 2).coerceAtLeast(0), actionWidth, actionSize.height)
        action.isVisible = actionWidth > 0

        var leftX = left
        val leftLimit = actionX - gap
        fun placeLeft(component: Component) {
            val size = component.preferredSize
            if (leftX + size.width > leftLimit) return
            component.setBounds(leftX, top + ((height - size.height) / 2).coerceAtLeast(0), size.width, size.height)
            component.isVisible = true
            leftX += size.width + gap
        }
        placeLeft(attach)
        placeLeft(command)

        var rightX = actionX - gap
        val rightLimit = leftX + gap
        fun placeRight(component: Component) {
            val size = component.preferredSize
            if (size.width <= 0) return
            val x = rightX - size.width
            if (x < rightLimit) return
            component.setBounds(x, top + ((height - size.height) / 2).coerceAtLeast(0), size.width, size.height)
            component.isVisible = true
            rightX = x - gap
        }
        placeRight(model)
        // A layout pass runs after setEffortChoices, so re-showing the chip here
        // unconditionally resurrected it (as a lone "Default") for models that
        // expose no levels. Only place it when there is something to pick.
        if (effortChipHasContent()) placeRight(effort)
        placeRight(mode)
        placeRight(token)
    }

    private fun makeToolButton(text: String, action: () -> Unit): JButton =
        JButton(text).apply {
            font = font.deriveFont(font.size2D - 2f)
            cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
            isContentAreaFilled = true
            isBorderPainted = true
            isFocusPainted = false
            isOpaque = true
            margin = Insets(0, 0, 0, 0)
            preferredSize = Dimension(preferredSize.width.coerceAtLeast(62), 30)
            addActionListener { action() }
            toolButtons += this
            addMouseListener(object : MouseAdapter() {
                override fun mouseEntered(e: MouseEvent) {
                    background = CLICKABLE_HOVER_BG
                }

                override fun mouseExited(e: MouseEvent) {
                    background = CLICKABLE_BG
                }
            })
        }

    private fun makeCompactToolButton(text: String, tooltip: String, action: () -> Unit): JButton =
        makeToolButton(text, action).apply {
            toolTipText = tooltip
            accessibleContext.accessibleName = tooltip
            preferredSize = Dimension(38, 30)
            minimumSize = Dimension(38, 30)
            compactToolButtons += this
        }

    private fun applyTheme() {
        background = PANEL_BG
        inputArea.background = INPUT_BG
        inputArea.foreground = INPUT_FG
        inputArea.caretColor = INPUT_FG

        toolButtons.forEach { button ->
            button.foreground = CLICKABLE_FG
            button.background = CLICKABLE_BG
            button.border = clickableBorder(horizontal = if (button in compactToolButtons) 8 else 12)
        }

        sendButton.foreground = SEND_FG
        sendButton.background = SEND_BG
        sendButton.border = clickableBorder(horizontal = 12)

        tokenLabel.foreground = TOKEN_FG
        modelLabel.foreground = MODEL_FG
        modelLabel.border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        approvalModeLabel.foreground = if (approvalModeLabel.isEnabled) MODEL_FG else DISABLED_FG
        approvalModeLabel.border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        effortLabel.foreground = if (effortLabel.isEnabled) MODEL_FG else DISABLED_FG
        effortLabel.border = BorderFactory.createEmptyBorder(5, 8, 5, 8)
        stopButton.foreground = STOP_FG
        stopButton.background = STOP_BG
        stopButton.border = clickableBorder(horizontal = 12)

        queueChips.applyTheme()
        contextChips.repaint()

        revalidate()
        repaint()
    }

    private fun clickableBorder(horizontal: Int) = BorderFactory.createCompoundBorder(
        BorderFactory.createLineBorder(CLICKABLE_BORDER, 1, true),
        BorderFactory.createEmptyBorder(5, horizontal, 5, horizontal),
    )

    companion object {
        // JBColor(亮色, 暗色)
        private val PANEL_BG get() = UIManager.getColor("Panel.background") ?: JBColor(0xF5F5F5, 0x1E1E1E)
        private val INPUT_BG get() = UIManager.getColor("TextArea.background") ?: JBColor(0xFFFFFF, 0x2D2D2D)
        private val INPUT_FG get() = UIManager.getColor("TextArea.foreground") ?: JBColor(0x333333, 0xD4D4D4)
        private val COMPOSER_BORDER get() = UIManager.getColor("Component.borderColor") ?: JBColor(0xC9C9C9, 0x454545)
        private val SECONDARY_FG = JBColor(0x5F6368, 0xA0A0A0)
        private val DISABLED_FG = JBColor(0x8A8F96, 0x6F7378)
        private val PLACEHOLDER_FG = JBColor(0x7A7F86, 0x8F949B)
        private val PLACEHOLDER_SECONDARY_FG = JBColor(0x9AA0A7, 0x70757C)
        private val TOKEN_FG = JBColor(0x999999, 0x666666)
        private val CLICKABLE_BG = JBColor(0xF7F7F7, 0x333333)
        private val CLICKABLE_HOVER_BG = JBColor(0xECECEC, 0x3D3D3D)
        private val CLICKABLE_BORDER = JBColor(0xD4D4D4, 0x4A4A4A)
        private val CLICKABLE_FG = JBColor(0x555555, 0xB8B8B8)
        private val MODEL_FG = JBColor(0x157A61, 0x56D6BF)
        private val SEND_BG = CLICKABLE_BG
        private val SEND_FG = JBColor(0x555555, 0xB8B8B8)
        private val STOP_BG = JBColor(0xF4DEDE, 0x4A2424)
        private val STOP_FG = JBColor(0xA52D2D, 0xF48771)
        private const val SEND_CARD = "send"
        private const val STOP_CARD = "stop"
    }
}

internal fun slashCommandPrefix(text: String): String? {
    if (!text.startsWith("/")) return null
    if (text.any { it.isWhitespace() }) return null
    return text
}

internal const val MIN_COMMAND_ROWS = 6
internal const val MAX_COMMAND_ROWS = 12
internal const val COMMAND_POPUP_RESERVED_HEIGHT = 160

/**
 * 屏幕上最多放得下多少行命令。弹窗高过屏幕时 Swing 会把它渲染成重型窗口，那是
 * "列表一闪而过"的起点（见 showCommandPopup 的说明），所以这里要么放得下，
 * 要么明确截断到放得下。
 */
internal fun commandRowsThatFit(availableHeight: Int, rowHeight: Int): Int {
    val usable = (availableHeight - COMMAND_POPUP_RESERVED_HEIGHT).coerceAtLeast(0)
    val row = rowHeight.coerceAtLeast(1)
    return (usable / row).coerceIn(MIN_COMMAND_ROWS, MAX_COMMAND_ROWS)
}

