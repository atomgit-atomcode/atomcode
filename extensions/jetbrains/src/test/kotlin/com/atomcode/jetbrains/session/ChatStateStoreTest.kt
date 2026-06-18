package com.atomcode.jetbrains.session

import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.MessageInfo
import com.atomcode.jetbrains.daemon.SessionDetail
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ChatStateStoreTest {
    private fun store(): ChatStateStore =
        ChatStateStore(
            initialState = ChatState(tabId = "tab-1"),
            ids = CountingIds(),
            clock = FixedClock(),
        )

    @Test
    fun `submit prompt appends user message and clears context and draft`() {
        val store = store()
        val context = ContextItemState(
            id = "ctx-1",
            path = "/repo/src/main.kt",
            displayName = "src/main.kt",
            language = "kotlin",
            selectionStartLine = 10,
            selectionEndLine = 12,
        )

        store.dispatch(ChatAction.DraftChanged("draft"))
        store.dispatch(ChatAction.AddContext(context))
        val state = store.dispatch(ChatAction.SubmitPrompt(" explain this "))

        assertEquals("", state.draft)
        assertTrue(state.pendingContext.isEmpty())
        assertIs<GenerationState.Streaming>(state.generation)
        val message = assertIs<MessageState.User>(state.messages.single())
        assertEquals("explain this", message.text)
        assertEquals(listOf("src/main.kt:10-12"), message.contextSummary)
    }

    @Test
    fun `text events append to the current streaming assistant message`() {
        val store = store()

        store.dispatch(ChatAction.SubmitPrompt("hello"))
        store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.Text("one ")))
        val state = store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.Text("two")))

        val assistant = assertIs<MessageState.Assistant>(state.messages.last())
        assertEquals("one two", assistant.rawMarkdown)
        assertEquals(AssistantStatus.Streaming, assistant.status)
    }

    @Test
    fun `done event completes streaming assistant message`() {
        val store = store()

        store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.Text("answer")))
        val state = store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.Done(tokens = 10, toolCalls = 0, sessionId = "s1")))

        val assistant = assertIs<MessageState.Assistant>(state.messages.single())
        assertEquals(AssistantStatus.Complete, assistant.status)
        assertIs<GenerationState.Idle>(state.generation)
        assertEquals("s1", state.session?.id)
    }

    @Test
    fun `tool events create and complete a tool call message`() {
        val store = store()

        store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.ToolStart("call-1", "bash", """{"cmd":"ls"}""")))
        store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.ToolOutput("file-a\n")))
        val state = store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.ToolResult("call-1", "bash", "file-b\n", true, 42)))

        val tool = assertIs<MessageState.ToolCall>(state.messages.single())
        assertEquals("call-1", tool.callId)
        assertEquals("bash", tool.name)
        assertEquals(ToolStatus.Success, tool.status)
        assertEquals(42, tool.durationMs)
        assertEquals(listOf("file-a\n", "file-b\n"), tool.outputChunks)
    }

    @Test
    fun `artifact content does not duplicate assistant text`() {
        val store = store()

        store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.Text("visible answer")))
        val state = store.dispatch(ChatAction.DaemonEventReceived(ChatEvent.ArtifactContent("artifact-1", "artifact body")))

        val assistant = assertIs<MessageState.Assistant>(state.messages.single())
        assertEquals("visible answer", assistant.rawMarkdown)
    }

    @Test
    fun `permission request inserts card state and waits for decision`() {
        val store = store()

        val state = store.dispatch(
            ChatAction.DaemonEventReceived(
                ChatEvent.PermissionRequest(
                    sessionId = "s1",
                    toolName = "bash",
                    reason = "needs shell",
                    callId = "call-1",
                    arguments = """{"cmd":"rm file"}""",
                ),
            ),
        )

        val permission = assertIs<MessageState.Permission>(state.messages.single())
        assertEquals("bash", permission.request.toolName)
        assertEquals(permission.request, state.activePermission)
        assertIs<GenerationState.WaitingPermission>(state.generation)
    }

    @Test
    fun `permission decision clears active permission`() {
        val store = store()
        val waiting = store.dispatch(
            ChatAction.DaemonEventReceived(
                ChatEvent.PermissionRequest("s1", "bash", "needs shell", "call-1", "{}"),
            ),
        )

        val state = store.dispatch(
            ChatAction.PermissionDecision(
                requestId = waiting.activePermission!!.requestId,
                decision = PermissionDecisionKind.AllowOnce,
            ),
        )

        assertNull(state.activePermission)
        assertIs<GenerationState.Streaming>(state.generation)
    }

    @Test
    fun `session loaded replaces messages and clears runtime state`() {
        val store = store()
        store.dispatch(ChatAction.QueuePrompt("queued"))

        val state = store.dispatch(
            ChatAction.SessionLoaded(
                SessionDetail(
                    id = "s1",
                    name = "Saved",
                    workingDir = "/repo",
                    projectHash = "hash",
                    messages = listOf(
                        MessageInfo("user", "question"),
                        MessageInfo("assistant", "answer"),
                    ),
                ),
            ),
        )

        assertEquals("s1", state.session?.id)
        assertTrue(state.queue.isEmpty())
        assertIs<GenerationState.Idle>(state.generation)
        assertEquals(2, state.messages.size)
        assertEquals("question", assertIs<MessageState.User>(state.messages[0]).text)
        assertEquals("answer", assertIs<MessageState.Assistant>(state.messages[1]).rawMarkdown)
    }
}

private class CountingIds : IdFactory {
    private var next = 0

    override fun next(prefix: String): String {
        next += 1
        return "$prefix-$next"
    }
}

private class FixedClock : Clock {
    override fun nowMillis(): Long = 123L
}
