package com.atomcode.jetbrains.ui

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowManager
import com.intellij.ui.content.ContentFactory

const val ATOMCODE_TOOL_WINDOW_ID = "AtomCode"

fun createAtomCodeChatContent(project: Project, toolWindow: ToolWindow, closeable: Boolean): AtomCodeChatPanel {
    val panel = AtomCodeChatPanel(project)
    val name = nextChatTabName(toolWindow)
    val content = ContentFactory.getInstance().createContent(panel, name, false).apply {
        isCloseable = closeable
        description = "AtomCode Chat"
        setDisposer(panel)
    }
    toolWindow.contentManager.addContent(content)
    toolWindow.contentManager.setSelectedContent(content)
    return panel
}

fun selectedAtomCodeChatPanel(project: Project): AtomCodeChatPanel? {
    val toolWindow = ToolWindowManager.getInstance(project).getToolWindow(ATOMCODE_TOOL_WINDOW_ID) ?: return null
    val selected = toolWindow.contentManager.selectedContent?.component as? AtomCodeChatPanel
    if (selected != null) return selected
    return toolWindow.contentManager.contents
        .asSequence()
        .mapNotNull { it.component as? AtomCodeChatPanel }
        .firstOrNull()
}

fun openAtomCodeChatTab(project: Project, newTab: Boolean = false, focusInput: Boolean = true) {
    ApplicationManager.getApplication().invokeLater {
        val toolWindow = ToolWindowManager.getInstance(project).getToolWindow(ATOMCODE_TOOL_WINDOW_ID) ?: return@invokeLater
        toolWindow.show()
        if (newTab || toolWindow.contentManager.contentCount == 0) {
            createAtomCodeChatContent(project, toolWindow, closeable = true)
        }
        val panel = selectedAtomCodeChatPanel(project)
        if (focusInput) panel?.focusInput()
    }
}

fun closeCurrentChatTab(project: Project) {
    ApplicationManager.getApplication().invokeLater {
        val toolWindow = ToolWindowManager.getInstance(project).getToolWindow(ATOMCODE_TOOL_WINDOW_ID) ?: return@invokeLater
        if (toolWindow.contentManager.contentCount <= 1) return@invokeLater
        val selected = toolWindow.contentManager.selectedContent ?: return@invokeLater
        toolWindow.contentManager.removeContent(selected, true)
    }
}

private fun nextChatTabName(toolWindow: ToolWindow): String {
    val count = toolWindow.contentManager.contentCount
    return if (count == 0) "Chat" else "Chat ${count + 1}"
}
