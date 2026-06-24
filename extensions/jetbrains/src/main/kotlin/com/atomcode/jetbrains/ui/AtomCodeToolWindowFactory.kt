package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.actions.openAtomCodeSettings
import com.atomcode.jetbrains.session.SessionWorkspace
import com.intellij.icons.AllIcons
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.openapi.wm.ex.ToolWindowManagerListener

class AtomCodeToolWindowFactory : ToolWindowFactory {
    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
        installEmptyToolWindowActivationListener(project, toolWindow)

        val workspace = SessionWorkspace.getInstance(project)
        val restoredTabs = workspace.restoredTabs()
        if (restoredTabs.isEmpty()) {
            createAtomCodeChatContent(project, toolWindow, closeable = true)
        } else {
            restoredTabs.forEach { restoreAtomCodeChatContent(project, toolWindow, it) }
            workspace.selectedTabId()?.let { selectedTabId ->
                toolWindow.contentManager.contents
                    .firstOrNull { contentTabId(it) == selectedTabId }
                    ?.let { toolWindow.contentManager.setSelectedContent(it) }
            }
        }

        toolWindow.setTitleActions(listOf(
            object : AnAction("Home", "Open AtomCode welcome and quick start", null) {
                override fun getActionUpdateThread() = ActionUpdateThread.BGT
                override fun actionPerformed(e: AnActionEvent) {
                    e.project?.let { openAtomCodeWelcomePage(it) }
                }
            },
            object : AnAction("New Tab", "Open a new chat tab", AllIcons.General.Add) {
                override fun getActionUpdateThread() = ActionUpdateThread.BGT
                override fun actionPerformed(e: AnActionEvent) {
                    e.project?.let { openAtomCodeChatTab(it, newTab = true) }
                }
            },
            object : AnAction("Settings", "Open AtomCode settings", AllIcons.General.GearPlain) {
                override fun getActionUpdateThread() = ActionUpdateThread.BGT
                override fun actionPerformed(e: AnActionEvent) {
                    e.project?.let { p ->
                        selectedAtomCodeChatPanel(p)?.showGearMenu() ?: p.openAtomCodeSettings()
                    }
                }
            },
        ))
    }

    private fun installEmptyToolWindowActivationListener(project: Project, toolWindow: ToolWindow) {
        if (toolWindow.component.getClientProperty("atomcode-empty-activation-listener-installed") == true) return
        toolWindow.component.putClientProperty("atomcode-empty-activation-listener-installed", true)

        project.messageBus.connect(project).subscribe(
            ToolWindowManagerListener.TOPIC,
            object : ToolWindowManagerListener {
                override fun toolWindowShown(shownToolWindow: ToolWindow) {
                    if (shownToolWindow !== toolWindow) return
                    ensureContentIfEmpty(project, shownToolWindow)
                }
            },
        )
    }

    private fun ensureContentIfEmpty(project: Project, toolWindow: ToolWindow) {
        if (toolWindow.contentManager.contentCount != 0) return
        ApplicationManager.getApplication().invokeLater {
            if (toolWindow.contentManager.contentCount == 0) {
                ensureAtomCodeChatContent(project, toolWindow)
            }
        }
    }
}
