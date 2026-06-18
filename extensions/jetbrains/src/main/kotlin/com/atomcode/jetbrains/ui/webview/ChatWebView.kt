package com.atomcode.jetbrains.ui.webview

import com.atomcode.jetbrains.store.ChatViewModel
import com.intellij.ui.jcef.JBCefBrowser
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.awt.BorderLayout
import javax.swing.JComponent
import javax.swing.JPanel

/**
 * JCEF 内嵌消息渲染视图。
 * Kotlin → React：render(viewModel) 调用 window.dispatchRender(json)
 */
class ChatWebView {
    private val json = Json { encodeDefaults = true }
    @Volatile var isReady = false
        private set
    lateinit var browser: JBCefBrowser
        private set

    fun createComponent(parent: JComponent): JComponent {
        browser = JBCefBrowser()
        // 从 classpath resource 加载 webview/index.html
        val url = javaClass.getResource("/webview/index.html")?.toExternalForm()
            ?: "about:blank"
        browser.loadURL(url)

        val panel = JPanel(BorderLayout())
        panel.add(browser.component, BorderLayout.CENTER)
        return panel
    }

    fun render(viewModel: ChatViewModel) {
        if (!isReady) return
        val jsonStr = json.encodeToString(viewModel)
        val escaped = jsonStr
            .replace("\\", "\\\\")
            .replace("'", "\\'")
        browser.cefBrowser.executeJavaScript(
            "window.dispatchRender('$escaped')",
            browser.cefBrowser.url, 0
        )
    }

    fun onReady() { isReady = true }
    fun onJsReadyTimeout() { isReady = true }
}
