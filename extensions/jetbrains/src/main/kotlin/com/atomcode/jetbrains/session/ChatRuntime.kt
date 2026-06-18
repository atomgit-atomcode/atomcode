package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.store.ChatStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

/**
 * 兼容包装器：桥接旧 ChatRuntime API → 新 ChatStore。
 */
class ChatRuntime(
    val tabId: String,
    val store: ChatStore,
) {
    constructor(tabId: String, client: DaemonClient) : this(
        tabId,
        ChatStore(tabId, client, CoroutineScope(Dispatchers.Default + SupervisorJob()))
    )

    // 旧代码兼容属性
    val state: ChatState get() = ChatState(tabId = tabId)

    // 旧代码兼容方法
    fun submitPrompt(text: String) = store.submitPrompt(text)
    fun stopGeneration() = store.stop()
    fun queuePrompt(text: String, id: String? = null) { store.submitPrompt(text) }
    fun addContext(item: Any?) {}
    fun clearContext() {}
    fun loadSession(detail: Any?) {}
    fun updateSession(session: Any?) {}
    fun updateDraft(text: String) {}
    fun removeQueuedPrompt(id: String) {}
    fun applyDaemonEvent(event: Any?) {}
    fun updateConnection(state: Any?) {}

    companion object {
        fun create(tabId: String, client: DaemonClient): ChatRuntime =
            ChatRuntime(tabId, client)
    }
}
