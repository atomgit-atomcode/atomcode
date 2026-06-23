package com.atomcode.jetbrains.actions

import com.atomcode.jetbrains.ui.AtomCodeChatPanel
import com.atomcode.jetbrains.ui.openAtomCodeChatTab
import com.atomcode.jetbrains.ui.selectedAtomCodeChatPanel
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.actionSystem.ActionUpdateThread

class OpenChatAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        e.presentation.isEnabled = e.getData(CommonDataKeys.PROJECT) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        val project = e.getData(CommonDataKeys.PROJECT) ?: return
        openAtomCodeChatTab(project)
    }
}

internal fun findChatPanel(project: com.intellij.openapi.project.Project): AtomCodeChatPanel? {
    return selectedAtomCodeChatPanel(project)
}
