package com.atomcode.jetbrains.actions

import com.atomcode.jetbrains.ui.ensureAtomCodeChatContent
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.wm.ToolWindowManager

class NewConversationAction : AnAction() {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        e.presentation.isEnabled = e.getData(CommonDataKeys.PROJECT) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        val project = e.getData(CommonDataKeys.PROJECT) ?: return
        ToolWindowManager.getInstance(project).getToolWindow("AtomCode")?.activate {
            val toolWindow = ToolWindowManager.getInstance(project).getToolWindow("AtomCode") ?: return@activate
            ensureAtomCodeChatContent(project, toolWindow).startNewConversation()
        }
    }
}
