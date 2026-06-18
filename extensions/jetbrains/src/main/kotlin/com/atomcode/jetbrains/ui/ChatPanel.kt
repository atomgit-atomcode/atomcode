package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.client.ConnectionState
import com.atomcode.jetbrains.store.ChatStore
import com.atomcode.jetbrains.store.SessionStore
import com.atomcode.jetbrains.store.ProviderStore
import com.atomcode.jetbrains.ui.webview.ChatWebView
import com.atomcode.jetbrains.ui.webview.HostBridge
import com.atomcode.jetbrains.ui.webview.RenderThrottler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import java.awt.BorderLayout
import javax.swing.JPanel

/**
 * 每个 tab 的主聊天面板。
 * 组合 Header + ChatWebView (JCEF) + InputPanel (Swing)。
 * 只负责布局和接线，不持有业务状态。
 */
class ChatPanel(
    val tabId: String,
    val chatStore: ChatStore,
    val sessionStore: SessionStore,
    val providerStore: ProviderStore,
    val connectionState: StateFlow<ConnectionState>,
    val clipboardHandler: (String) -> Unit,
    val fileOpenHandler: (String, Int?) -> Unit,
    val onNewTab: () -> Unit,
) : JPanel(BorderLayout()) {

    private val scope = CoroutineScope(Dispatchers.Main + SupervisorJob())
    private val chatWebView = ChatWebView()
    private val hostBridge = HostBridge(chatWebView, chatStore, clipboardHandler, fileOpenHandler)
    private val throttler = RenderThrottler { vm -> chatWebView.render(vm) }

    init {
        // Header: 连接状态 + model selector
        val header = javax.swing.JLabel("AtomCode")
        header.horizontalAlignment = javax.swing.SwingConstants.CENTER
        add(header, BorderLayout.NORTH)

        // Center: JCEF webview
        add(chatWebView.createComponent(this), BorderLayout.CENTER)

        // South: 输入区（后续集成 InputPanel）
        val inputPlaceholder = javax.swing.JLabel("Input area — to be wired")
        inputPlaceholder.horizontalAlignment = javax.swing.SwingConstants.CENTER
        add(inputPlaceholder, BorderLayout.SOUTH)

        hostBridge.install()

        // 观察 store 状态 → 渲染
        scope.launch {
            throttler.observe(chatStore.state)
        }

        // 观察连接状态
        scope.launch {
            connectionState.collect { state ->
                val status = when (state) {
                    is ConnectionState.Idle -> "Idle"
                    is ConnectionState.Checking -> "Connecting..."
                    is ConnectionState.Starting -> "Connecting..."
                    is ConnectionState.Restarting -> "Connecting..."
                    is ConnectionState.Ready -> "Connected (${state.version})"
                    is ConnectionState.Error -> "Error: ${state.message}"
                }
                header.text = "AtomCode — $status"
            }
        }
    }

    fun dispose() {
        scope.cancel()
    }
}
