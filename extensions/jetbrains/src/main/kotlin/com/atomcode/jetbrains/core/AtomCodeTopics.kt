package com.atomcode.jetbrains.core

import com.atomcode.jetbrains.context.EditorContextState
import com.atomcode.jetbrains.daemon.DaemonSupervisorState
import com.atomcode.jetbrains.session.ChatState
import com.intellij.util.messages.Topic

interface DaemonStateListener {
    fun daemonStateChanged(state: DaemonSupervisorState)
}

interface ChatStateListener {
    fun chatStateChanged(tabId: String, state: ChatState)
}

interface EditorContextListener {
    fun editorContextChanged(state: EditorContextState)
}

object AtomCodeTopics {
    @Topic.ProjectLevel
    val DAEMON_STATE: Topic<DaemonStateListener> =
        Topic.create("AtomCode daemon state", DaemonStateListener::class.java)

    @Topic.ProjectLevel
    val CHAT_STATE: Topic<ChatStateListener> =
        Topic.create("AtomCode chat state", ChatStateListener::class.java)

    @Topic.ProjectLevel
    val EDITOR_CONTEXT: Topic<EditorContextListener> =
        Topic.create("AtomCode editor context", EditorContextListener::class.java)
}
