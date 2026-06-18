package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.session.SessionWorkspace
import com.intellij.icons.AllIcons
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.project.Project
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory

class AtomCodeToolWindowFactory : ToolWindowFactory {
    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
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
                        selectedAtomCodeChatPanel(p)?.showGearMenu()
                    }
                }
            },
        ))
    }
}
