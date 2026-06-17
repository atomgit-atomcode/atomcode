package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.DaemonSupervisorState
import com.atomcode.jetbrains.daemon.SessionDetail
import com.atomcode.jetbrains.services.SessionRefView

class ChatRuntime(
    val tabId: String,
    initialState: ChatState = ChatState(tabId = tabId),
    ids: IdFactory = IdFactory.uuid(),
    clock: Clock = Clock.system(),
) {
    val store = ChatStateStore(initialState, ids, clock)

    val state: ChatState
        get() = store.state

    fun updateDraft(text: String): ChatState =
        store.dispatch(ChatAction.DraftChanged(text))

    fun submitPrompt(text: String): ChatState =
        store.dispatch(ChatAction.SubmitPrompt(text))

    fun queuePrompt(text: String, id: String? = null): ChatState =
        store.dispatch(ChatAction.QueuePrompt(text, id))

    fun removeQueuedPrompt(id: String): ChatState =
        store.dispatch(ChatAction.RemoveQueuedPrompt(id))

    fun addContext(item: ContextItemState): ChatState =
        store.dispatch(ChatAction.AddContext(item))

    fun clearContext(): ChatState =
        store.dispatch(ChatAction.ClearContext)

    fun applyDaemonEvent(event: ChatEvent): ChatState =
        store.dispatch(ChatAction.DaemonEventReceived(event))

    fun loadSession(detail: SessionDetail): ChatState =
        store.dispatch(ChatAction.SessionLoaded(detail))

    fun updateSession(session: SessionRefView): ChatState =
        store.dispatch(ChatAction.SessionRefUpdated(session))

    fun updateConnection(state: DaemonSupervisorState): ChatState =
        store.dispatch(ChatAction.ConnectionChanged(state))
}
