package com.atomcode.jetbrains.ui.webview

import com.atomcode.jetbrains.store.ChatStore
import com.atomcode.jetbrains.store.PermissionDecisionKind
import com.atomcode.jetbrains.ui.jcef.JBCefQueryHandlers
import com.google.gson.Gson
import com.google.gson.annotations.SerializedName

/**
 * React → Kotlin 回调桥接。
 * 通过 JBCefJSQuery 接收 UI 回调消息。
 */
class HostBridge(
    private val chatWebView: ChatWebView,
    private val store: ChatStore,
    private val onCopyCode: (String) -> Unit,
    private val onOpenFile: (String, Int?) -> Unit,
) {
    private val gson = Gson()
    private var jsQuery: Any? = null

    data class CallbackMsg(
        val type: String? = null,
        val code: String? = null,
        val path: String? = null,
        val line: Int? = null,
        @SerializedName("call_id") val callId: String? = null,
        val decision: String? = null,
    )

    fun install() {
        val query = JBCefQueryHandlers.create(chatWebView.browser) { rawJson ->
            try {
                val msg = gson.fromJson(rawJson, CallbackMsg::class.java)
                when (msg.type) {
                    "ready" -> chatWebView.onReady()
                    "copy_code" -> msg.code?.let { onCopyCode(it) }
                    "open_file" -> msg.path?.let { onOpenFile(it, msg.line) }
                    "permission_decision" -> {
                        val decision = when (msg.decision) {
                            "allow" -> PermissionDecisionKind.Allow
                            "deny" -> PermissionDecisionKind.Deny
                            "always_allow" -> PermissionDecisionKind.AlwaysAllow
                            "allow_persist" -> PermissionDecisionKind.AllowPersist
                            else -> null
                        }
                        if (decision != null && msg.callId != null) {
                            store.submitPermission(msg.callId, decision)
                        }
                    }
                    "scroll_complete" -> { /* no-op */ }
                }
            } catch (_: Exception) {}
        }
        jsQuery = query
        // 将 jsQuery 注入到 JS 全局作用域
        chatWebView.browser.cefBrowser.executeJavaScript(
            "window.jsQuery = function(msg) { ${JBCefQueryHandlers.inject(query, "msg")} }",
            chatWebView.browser.cefBrowser.url, 0
        )
    }
}
