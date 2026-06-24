package com.atomcode.jetbrains.settings

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.PersistentStateComponent
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.State
import com.intellij.openapi.components.Storage

enum class AtomCodeContextLevel {
    Minimal,
    CurrentFile,
    ProjectContext,
}

data class AtomCodeSettings(
    var daemonBinaryPath: String = "",
    var host: String = "127.0.0.1",
    var port: Int = 13456,
    var autoStart: Boolean = true,
    var requestTimeoutMs: Int = 30_000,
    var autoSaveBeforeRead: Boolean = true,
    var contextLevel: AtomCodeContextLevel = AtomCodeContextLevel.Minimal,
    var allowSelectedTextContext: Boolean = true,
    var sendRelativePathWithSelection: Boolean = true,
    var sendWithCtrlEnter: Boolean = false,
    var chatFontSize: Int = 13,
    var welcomePageShown: Boolean = false,
)

@Service(Service.Level.APP)
@State(name = "AtomCodeSettings", storages = [Storage("atomcode.xml")])
class AtomCodeSettingsState : PersistentStateComponent<AtomCodeSettings> {
    private var state = AtomCodeSettings()

    override fun getState(): AtomCodeSettings = state

    override fun loadState(state: AtomCodeSettings) {
        this.state = state.normalized()
    }

    fun update(block: (AtomCodeSettings) -> Unit) {
        val next = state.copy()
        block(next)
        state = next.normalized()
    }

    companion object {
        fun getInstance(): AtomCodeSettingsState =
            ApplicationManager.getApplication().getService(AtomCodeSettingsState::class.java)
    }
}

internal fun AtomCodeSettings.normalized(): AtomCodeSettings {
    if (host.isBlank()) host = "127.0.0.1"
    if (port <= 0) port = 13456
    if (requestTimeoutMs <= 0) requestTimeoutMs = 30_000
    if (chatFontSize <= 0) chatFontSize = 13
    return this
}
