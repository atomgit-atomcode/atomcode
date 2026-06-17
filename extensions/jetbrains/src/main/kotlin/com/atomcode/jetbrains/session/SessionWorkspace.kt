package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.persistence.AtomCodeProjectWorkspaceState
import com.atomcode.jetbrains.persistence.WorkspaceTabState
import com.intellij.openapi.Disposable
import com.intellij.openapi.components.Service
import com.intellij.openapi.project.Project
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

@Service(Service.Level.PROJECT)
class SessionWorkspace(private val project: Project) : Disposable {
    private val workspaceState = AtomCodeProjectWorkspaceState.getInstance(project)
    private val runtimes = ConcurrentHashMap<String, ChatRuntime>()

    fun createRuntime(title: String = "Chat"): ChatRuntime {
        val tabId = "tab-${UUID.randomUUID()}"
        val runtime = ChatRuntime(tabId)
        runtimes[tabId] = runtime
        workspaceState.upsertTab(WorkspaceTabState(tabId = tabId, title = title))
        workspaceState.selectTab(tabId)
        return runtime
    }

    fun createRuntimeForRestoredTab(tab: WorkspaceTabState): ChatRuntime {
        val runtime = runtimes[tab.tabId] ?: ChatRuntime(
            tab.tabId,
            initialState = ChatState(tabId = tab.tabId, draft = tab.draft),
        )
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

    fun restoredTabs(): List<WorkspaceTabState> =
        workspaceState.state.tabs.toList()

    fun selectedTabId(): String? =
        workspaceState.state.selectedTabId

    private fun restoreRuntime(tabId: String): ChatRuntime? {
        val tab = workspaceState.state.tabs.firstOrNull { it.tabId == tabId } ?: return null
        val runtime = ChatRuntime(tab.tabId, initialState = ChatState(tabId = tab.tabId, draft = tab.draft))
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
