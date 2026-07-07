package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.client.ConnectionState
import com.atomcode.jetbrains.ide.ClipboardService
import com.atomcode.jetbrains.ide.EditorContext
import com.atomcode.jetbrains.protocol.ApprovalMode
import com.atomcode.jetbrains.store.ChatStore
import com.atomcode.jetbrains.store.SessionStore
import com.atomcode.jetbrains.store.ProviderStore
import com.atomcode.jetbrains.ui.input.InputPanel
import com.atomcode.jetbrains.ui.webview.ChatWebView
import com.atomcode.jetbrains.ui.webview.HostBridge
import com.atomcode.jetbrains.ui.webview.RenderThrottler
import com.intellij.openapi.project.Project
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import java.awt.BorderLayout
import javax.swing.JLabel
import javax.swing.JComboBox
import javax.swing.JPanel
import javax.swing.SwingConstants

/**
 * Per-tab 主聊天面板。
 * 组合 Header + ChatWebView (JCEF) + InputPanel (Swing)。
 * 只负责布局和接线，不持有业务状态。
 */
class ChatPanel(
    val tabId: String,
    val chatStore: ChatStore,
    val sessionStore: SessionStore,
    val providerStore: ProviderStore,
    val connectionState: StateFlow<ConnectionState>,
    private val project: Project,
) : JPanel(BorderLayout()) {

    private val scope = CoroutineScope(Dispatchers.Main + SupervisorJob())
    private val editorContext = EditorContext(project)
    private val chatWebView = ChatWebView()
    private val throttler = RenderThrottler { vm -> chatWebView.render(vm) }

    private val headerLabel = JLabel("AtomCode").apply {
        horizontalAlignment = SwingConstants.CENTER
    }
    private val modePicker = JComboBox(ApprovalMode.values()).apply {
        selectedItem = ApprovalMode.Build
        toolTipText = "Approval mode"
    }
    private var syncingModePicker = false

    private val inputPanel = InputPanel(
        onSend = { text ->
            chatStore.submitPrompt(text, workingDir = project.basePath)
            true
        },
        onStop = { chatStore.stop() },
        onAttach = {
            // 打开 IntelliJ 文件选择器 → 附加为上下文
            val files = com.intellij.openapi.fileChooser.FileChooser.chooseFiles(
                com.intellij.openapi.fileChooser.FileChooserDescriptorFactory.createSingleFileNoJarsDescriptor(),
                project, null
            )
            files.forEach { f ->
                chatStore.submitPrompt(
                    text = "Attached: ${f.name}",
                    contextFiles = listOf(f.path),
                    workingDir = project.basePath
                )
            }
        },
        onSlashCommand = { /* slash command picker - future */ },
        onClearContext = { /* clear context chips */ },
        onRemoveContext = { /* remove single context chip */ },
        onModelSelect = { /* model selector dialog - future */ },
        onPasteFromClipboard = { false },
    )

    private val hostBridge = HostBridge(
        chatWebView = chatWebView,
        store = chatStore,
        onCopyCode = { code -> ClipboardService.copyToClipboard(code) },
        onOpenFile = { path, line ->
            val vf = com.intellij.openapi.vfs.LocalFileSystem.getInstance().findFileByPath(path)
            if (vf != null) {
                val offset = if (line != null) {
                    val doc = com.intellij.openapi.fileEditor.FileDocumentManager.getInstance().getDocument(vf)
                    doc?.getLineStartOffset((line - 1).coerceAtLeast(0)) ?: 0
                } else 0
                com.intellij.openapi.fileEditor.FileEditorManager.getInstance(project)
                    .openTextEditor(
                        com.intellij.openapi.fileEditor.OpenFileDescriptor(project, vf, offset),
                        true
                    )
            }
        },
    )

    init {
        val header = JPanel(BorderLayout()).apply {
            add(headerLabel, BorderLayout.CENTER)
            add(modePicker, BorderLayout.EAST)
        }
        add(header, BorderLayout.NORTH)
        add(chatWebView.createComponent(this), BorderLayout.CENTER)
        add(inputPanel, BorderLayout.SOUTH)

        modePicker.addActionListener {
            if (syncingModePicker) return@addActionListener
            (modePicker.selectedItem as? ApprovalMode)?.let(chatStore::setApprovalMode)
        }

        hostBridge.install()

        // 观察 store 状态 → 渲染
        scope.launch { throttler.observe(chatStore.state) }
        scope.launch {
            chatStore.state.collect { state ->
                val mode = when (state.approvalMode) {
                    ApprovalMode.Plan.wire -> ApprovalMode.Plan
                    ApprovalMode.Bypass.wire -> ApprovalMode.Bypass
                    else -> ApprovalMode.Build
                }
                syncingModePicker = true
                try {
                    if (modePicker.selectedItem != mode) {
                        modePicker.selectedItem = mode
                    }
                    modePicker.isEnabled = state.pendingApprovalMode == null
                } finally {
                    syncingModePicker = false
                }
            }
        }

        // 观察连接状态
        scope.launch {
            connectionState.collect { state ->
                headerLabel.text = when (state) {
                    is ConnectionState.Idle -> "AtomCode — Idle"
                    is ConnectionState.Checking -> "AtomCode — Connecting..."
                    is ConnectionState.Starting -> "AtomCode — Connecting..."
                    is ConnectionState.Restarting -> "AtomCode — Reconnecting..."
                    is ConnectionState.Ready -> "AtomCode — Connected (${state.version})"
                    is ConnectionState.Error -> "AtomCode — ${state.message}"
                }
            }
        }
    }

    fun dispose() {
        scope.cancel()
    }
}
