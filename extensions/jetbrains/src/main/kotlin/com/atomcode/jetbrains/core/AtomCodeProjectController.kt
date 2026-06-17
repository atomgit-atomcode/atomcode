package com.atomcode.jetbrains.core

import com.atomcode.jetbrains.session.ChatRuntime
import com.atomcode.jetbrains.session.SessionWorkspace
import com.atomcode.jetbrains.persistence.WorkspaceTabState
import com.intellij.openapi.Disposable
import com.intellij.openapi.components.Service
import com.intellij.openapi.project.Project

@Service(Service.Level.PROJECT)
class AtomCodeProjectController(private val project: Project) : Disposable {
    val sessions: SessionWorkspace
        get() = SessionWorkspace.getInstance(project)

    fun createChatRuntime(title: String = "Chat"): ChatRuntime =
        sessions.createRuntime(title)

    fun createRestoredChatRuntime(tab: WorkspaceTabState): ChatRuntime =
        sessions.createRuntimeForRestoredTab(tab)

    fun selectChatRuntime(tabId: String) {
        sessions.select(tabId)
    }

    fun closeChatRuntime(tabId: String) {
        sessions.close(tabId)
    }

    override fun dispose() = Unit

    companion object {
        fun getInstance(project: Project): AtomCodeProjectController =
            project.getService(AtomCodeProjectController::class.java)
    }
}
