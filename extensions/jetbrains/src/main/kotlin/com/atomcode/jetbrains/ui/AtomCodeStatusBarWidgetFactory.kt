package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.daemon.ConnectionState
import com.atomcode.jetbrains.services.AtomCodeProjectService
import com.intellij.openapi.project.Project
import com.intellij.openapi.wm.StatusBar
import com.intellij.openapi.wm.StatusBarWidget
import com.intellij.openapi.wm.StatusBarWidgetFactory
import com.intellij.util.Consumer
import java.awt.event.MouseEvent
import java.beans.PropertyChangeListener
import javax.swing.SwingUtilities

class AtomCodeStatusBarWidgetFactory : StatusBarWidgetFactory {
    override fun getId(): String = AtomCodeStatusBarWidget.ID

    override fun getDisplayName(): String = "AtomCode"

    override fun isAvailable(project: Project): Boolean = true

    override fun createWidget(project: Project): StatusBarWidget = AtomCodeStatusBarWidget(project)

    override fun disposeWidget(widget: StatusBarWidget) {
        widget.dispose()
    }

    override fun canBeEnabledOn(statusBar: StatusBar): Boolean = true
}

private class AtomCodeStatusBarWidget(private val project: Project) : StatusBarWidget, StatusBarWidget.TextPresentation {
    private val service = AtomCodeProjectService.getInstance(project)
    private var statusBar: StatusBar? = null
    private val listener = PropertyChangeListener {
        SwingUtilities.invokeLater {
            statusBar?.updateWidget(ID)
        }
    }

    init {
        service.addConnectionListener(listener)
    }

    override fun ID(): String = ID

    override fun install(statusBar: StatusBar) {
        this.statusBar = statusBar
    }

    override fun dispose() {
        service.removeConnectionListener(listener)
        statusBar = null
    }

    override fun getPresentation(): StatusBarWidget.WidgetPresentation = this

    override fun getText(): String =
        when (service.connectionState) {
            is ConnectionState.Ready -> "AtomCode"
            ConnectionState.Idle -> "AtomCode ○"
            ConnectionState.CheckingDaemon,
            ConnectionState.StartingDaemon,
            ConnectionState.Connecting,
            ConnectionState.SyncingProject,
            ConnectionState.CheckingProvider -> "AtomCode ..."
            is ConnectionState.SetupRequired,
            is ConnectionState.ProviderMissing,
            is ConnectionState.Error -> "AtomCode !"
        }

    override fun getTooltipText(): String =
        when (val state = service.connectionState) {
            is ConnectionState.Ready -> "AtomCode: Connected (${state.daemonVersion}). Click to open chat."
            ConnectionState.Idle -> "AtomCode: Not connected. Click to open chat."
            ConnectionState.CheckingDaemon -> "AtomCode: Checking daemon..."
            ConnectionState.StartingDaemon -> "AtomCode: Starting daemon..."
            ConnectionState.Connecting -> "AtomCode: Connecting..."
            ConnectionState.SyncingProject -> "AtomCode: Syncing project..."
            ConnectionState.CheckingProvider -> "AtomCode: Checking provider..."
            is ConnectionState.SetupRequired -> "AtomCode: Setup required - ${state.reason}"
            is ConnectionState.ProviderMissing -> "AtomCode: Provider missing"
            is ConnectionState.Error -> "AtomCode: ${state.message}"
        }

    override fun getAlignment(): Float = 0.5f

    override fun getClickConsumer(): Consumer<MouseEvent>? =
        Consumer {
            openAtomCodeChatTab(project)
        }

    companion object {
        const val ID = "AtomCodeStatus"
    }
}
