package com.atomcode.jetbrains.core

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.session.ChatRuntime
import com.atomcode.jetbrains.session.SessionWorkspace
import com.atomcode.jetbrains.store.ChatStore
import com.atomcode.jetbrains.store.ProviderStore
import com.atomcode.jetbrains.store.SessionStore
import com.atomcode.jetbrains.persistence.WorkspaceTabState
import com.intellij.openapi.Disposable
import com.intellij.openapi.components.Service
import com.intellij.openapi.project.Project
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

@Service(Service.Level.PROJECT)
class AtomCodeProjectController(private val project: Project) : Disposable {
    private val scope = CoroutineScope(Dispatchers.Default + SupervisorJob())

    val sessions: SessionWorkspace
        get() = SessionWorkspace.getInstance(project)

    // 新架构 stores（project 级单例）
    val daemonClient: DaemonClient by lazy {
        DaemonClient(
            baseUrl = "http://127.0.0.1:13456",
            token = com.atomcode.jetbrains.security.AtomCodeTokenFactory.createToken(),
        )
    }
    val sessionStore: SessionStore by lazy { SessionStore(daemonClient, scope) }
    val providerStore: ProviderStore by lazy { ProviderStore(daemonClient, scope) }

    // Active tab's ChatStore (set by ChatPanel)
    var activeChatStore: ChatStore? = null

    fun createChatRuntime(title: String): ChatRuntime {
        val tabId = java.util.UUID.randomUUID().toString()
        return ChatRuntime.create(tabId, daemonClient)
    }

    fun createChatStore(tabId: String): ChatStore {
        val store = ChatStore(tabId, daemonClient, scope)
        activeChatStore = store
        return store
    }

    fun createRestoredChatRuntime(tab: WorkspaceTabState) = sessions.createRuntimeForRestoredTab(tab, daemonClient)

    fun selectChatRuntime(tabId: String) { sessions.select(tabId) }

    fun closeChatRuntime(tabId: String) { sessions.close(tabId) }

    fun openInNewTab(sessionId: String) {
        // 由 ToolWindow 处理
    }

    override fun dispose() = Unit

    companion object {
        fun getInstance(project: Project): AtomCodeProjectController =
            project.getService(AtomCodeProjectController::class.java)
    }
}
