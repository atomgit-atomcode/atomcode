package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.persistence.AtomCodeProjectWorkspaceState
import com.atomcode.jetbrains.persistence.WorkspaceTabState
import com.atomcode.jetbrains.services.SessionRefView
import com.intellij.openapi.Disposable
import com.intellij.openapi.components.Service
import com.intellij.openapi.project.Project
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

@Service(Service.Level.PROJECT)
class SessionWorkspace(private val project: Project) : Disposable {
    private val workspaceState = AtomCodeProjectWorkspaceState.getInstance(project)
    private val runtimes = ConcurrentHashMap<String, ChatRuntime>()
    private val defaultClient: DaemonClient by lazy {
        DaemonClient(baseUrl = "http://127.0.0.1:13456")
    }

    fun createRuntime(title: String = "Chat", client: DaemonClient): ChatRuntime {
        val tabId = "tab-${UUID.randomUUID()}"
        val runtime = ChatRuntime(tabId, client)
        runtimes[tabId] = runtime
        workspaceState.upsertTab(WorkspaceTabState(tabId = tabId, title = title))
        workspaceState.selectTab(tabId)
        return runtime
    }

    fun createRuntimeForRestoredTab(tab: WorkspaceTabState, client: DaemonClient): ChatRuntime {
        val runtime = runtimes[tab.tabId] ?: ChatRuntime(tab.tabId, client)
        runtimes[tab.tabId] = runtime
        workspaceState.upsertTab(tab)
        return runtime
    }

    fun runtime(tabId: String): ChatRuntime? =
        runtimes[tabId] ?: restoreRuntime(tabId)

    fun select(tabId: String) {
        workspaceState.selectTab(tabId)
    }

    fun close(tabId: String) {
        runtimes.remove(tabId)
        workspaceState.removeTab(tabId)
    }

    fun updateRuntimeSession(runtime: ChatRuntime) {
        val session = runtime.state.session ?: return
        workspaceState.upsertTab(
            WorkspaceTabState(
                tabId = runtime.tabId,
                sessionId = session.id,
                projectHash = session.projectHash,
                workingDir = session.workingDir,
                title = session.name.ifBlank { session.id.take(8) },
                draft = runtime.state.draft,
            ),
        )
    }

    fun updateTabSession(tabId: String, session: SessionRefView) {
        val existing = workspaceState.state.tabs.firstOrNull { it.tabId == tabId }
        workspaceState.upsertTab(
            WorkspaceTabState(
                tabId = tabId,
                sessionId = session.id,
                projectHash = session.projectHash,
                workingDir = session.workingDir,
                title = session.name.ifBlank { existing?.title ?: session.id.take(8) },
                draft = existing?.draft.orEmpty(),
            ),
        )
    }

    fun restoredTabs(): List<WorkspaceTabState> =
        workspaceState.state.tabs.toList()

    fun selectedTabId(): String? =
        workspaceState.state.selectedTabId

    private fun restoreRuntime(tabId: String): ChatRuntime? {
        val tab = workspaceState.state.tabs.firstOrNull { it.tabId == tabId } ?: return null
        val runtime = ChatRuntime(tab.tabId, defaultClient)
        runtimes[tabId] = runtime
        return runtime
    }

    override fun dispose() {
        runtimes.clear()
    }

    companion object {
        fun getInstance(project: Project): SessionWorkspace =
            project.getService(SessionWorkspace::class.java)
    }
}
