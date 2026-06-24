package com.atomcode.jetbrains.ui.input

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
import javax.swing.MenuSelectionManager
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
    private val onSend: (String) -> Unit,
    private val onStop: () -> Unit,
    private val onAttach: () -> Unit,
    private val onSlashCommand: () -> Unit,
    private val onClearContext: () -> Unit,
    private val onRemoveContext: (ChatContextItem) -> Unit,
    private val onModelSelect: () -> Unit,
    private val onPasteFromClipboard: (Transferable?) -> Boolean,
) : JPanel(BorderLayout()), DataProvider, PasteProvider {

    private var slashTriggerConsumed = false
    private var commandPopup: JPopupMenu? = null
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
            add(modelLabel)
            add(actionPanel)
        }

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

    fun showCommandPopup(menu: JPopupMenu) {
        commandPopup?.isVisible = false
        commandPopup = menu
        menu.isFocusable = false
        menu.components.forEach { it.isFocusable = false }
        menu.show(inputArea, 0, -menu.preferredSize.height)
        filterCommandPopup(commandPrefix())
        selectCommandMenuItem(0)
        inputArea.requestFocusInWindow()
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
                if (!moveCommandSelection(-1)) {
                    inputArea.transferFocusBackward()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("DOWN"), "atomcode-command-down")
        inputArea.actionMap.put("atomcode-command-down", object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (!moveCommandSelection(1)) {
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
                if (!activateSelectedCommand()) {
                    inputArea.transferFocus()
                }
            }
        })

        inputArea.inputMap.put(KeyStroke.getKeyStroke("ENTER"), enterAction)
        inputArea.actionMap.put(enterAction, object : AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                if (activateSelectedCommand()) return
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

    private fun showSlashCommandsFromButton() {
        if (commandPrefix() == null) {
            slashTriggerConsumed = true
            inputArea.text = "/"
            inputArea.caretPosition = inputArea.document.length
        }
        onSlashCommand()
        inputArea.requestFocusInWindow()
    }

    private fun fireSend() {
        val text = inputArea.text.trim()
        if (text.isNotEmpty()) {
            inputArea.text = ""
            onSend(text)
        }
    }

    private fun hideCommandPopup() {
        commandPopup?.isVisible = false
        MenuSelectionManager.defaultManager().clearSelectedPath()
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
            commandPopup?.isVisible = false
            return
        }

        if (commandPopup?.isVisible == true) {
            filterCommandPopup(prefix)
            return
        }

        if (slashTriggerConsumed) return

        slashTriggerConsumed = true
        SwingUtilities.invokeLater {
            val currentPrefix = commandPrefix()
            if (currentPrefix != null) {
                onSlashCommand()
                SwingUtilities.invokeLater {
                    filterCommandPopup(currentPrefix)
                    selectCommandMenuItem(0)
                }
            }
        }
    }

    private fun commandPrefix(): String? {
        return slashCommandPrefix(inputArea.text)
    }

    private fun commandItems(): List<JMenuItem> =
        commandPopup
            ?.components
            ?.filterIsInstance<JMenuItem>()
            .orEmpty()

    private fun visibleCommandItems(): List<JMenuItem> =
        commandItems().filter { it.isVisible }

    private fun filterCommandPopup(prefix: String?) {
        val popup = commandPopup?.takeIf { it.isVisible } ?: return
        val normalizedPrefix = prefix.orEmpty().lowercase()
        val items = commandItems()
        items.forEach { item ->
            item.isVisible = normalizedPrefix.isBlank() ||
                item.text.lowercase().startsWith(normalizedPrefix)
        }
        popup.pack()
        if (items.any { it.isVisible }) {
            selectCommandMenuItem(0)
        } else {
            MenuSelectionManager.defaultManager().clearSelectedPath()
        }
    }

    private fun selectedCommandIndex(items: List<JMenuItem>): Int {
        val selected = MenuSelectionManager.defaultManager().selectedPath.lastOrNull() as? JMenuItem
        val selectedIndex = items.indexOf(selected)
        return selectedIndex.takeIf { it >= 0 } ?: 0
    }

    private fun selectCommandMenuItem(index: Int): Boolean {
        val popup = commandPopup?.takeIf { it.isVisible } ?: return false
        val items = visibleCommandItems()
        if (items.isEmpty()) return false
        val selected = index.coerceIn(items.indices)
        MenuSelectionManager.defaultManager().selectedPath = arrayOf<MenuElement>(popup, items[selected])
        return true
    }

    private fun moveCommandSelection(delta: Int): Boolean {
        val popup = commandPopup?.takeIf { it.isVisible } ?: return false
        val items = visibleCommandItems()
        if (items.isEmpty()) return false
        val next = (selectedCommandIndex(items) + delta).floorMod(items.size)
        MenuSelectionManager.defaultManager().selectedPath = arrayOf<MenuElement>(popup, items[next])
        return true
    }

    private fun activateSelectedCommand(): Boolean {
        val popup = commandPopup?.takeIf { it.isVisible } ?: return false
        val items = visibleCommandItems()
        if (items.isEmpty()) return false
        val selected = items[selectedCommandIndex(items)]
        selected.doClick(0)
        hideCommandPopup()
        return true
    }

    private fun Int.floorMod(divisor: Int): Int =
        ((this % divisor) + divisor) % divisor

    private fun layoutToolbar(
        toolbar: JPanel,
        attach: Component,
        command: Component,
        token: Component,
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

        listOf(attach, command, token, model, action).forEach { it.isVisible = false }

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
