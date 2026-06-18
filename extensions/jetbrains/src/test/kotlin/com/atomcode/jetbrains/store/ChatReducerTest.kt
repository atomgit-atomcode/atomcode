package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.protocol.*
import kotlinx.collections.immutable.persistentListOf
import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test

class ChatReducerTest {
    private val ids: IdGenerator = { prefix -> "$prefix-1" }

    @Test
    fun `SubmitPrompt adds user message and sets Streaming`() {
        val state = ChatState(tabId = "t1")
        val action = ChatAction.SubmitPrompt(text = "hello")
        val newState = reduce(state, action, ids)
        assertEquals(2, newState.messages.size)
        assertTrue(newState.messages[0] is Message.User)
        assertEquals("hello", (newState.messages[0] as Message.User).text)
        assertTrue(newState.generation is GenerationState.Streaming)
    }

    @Test
    fun `Text event appends to assistant markdown`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(
                Message.User(id = "u1", text = "hello"),
                Message.Assistant(id = "a1", markdown = "Hello"),
            )
        )
        val newState = reduce(state, ChatAction.DaemonEvent(ChatEvent.Text(content = " World")), ids)
        val last = newState.messages.last() as Message.Assistant
        assertEquals("Hello World", last.markdown)
    }

    @Test
    fun `Reasoning event appends to assistant reasoning`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(id = "a1", reasoning = "Let me")),
        )
        val newState = reduce(state, ChatAction.DaemonEvent(ChatEvent.Reasoning(content = " think...")), ids)
        assertEquals("Let me think...", (newState.messages.last() as Message.Assistant).reasoning)
    }

    @Test
    fun `ToolBatchStarted adds queued tool calls`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(id = "a1")),
        )
        val event = ChatEvent.ToolBatchStarted(listOf(
            ToolBatchCall(id = "t1", name = "read_file", arguments = "{}"),
            ToolBatchCall(id = "t2", name = "write_to_file", arguments = "{}"),
        ))
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        val last = newState.messages.last() as Message.Assistant
        assertEquals(2, last.toolCalls.size)
        assertEquals(ToolStatus.Queued, last.toolCalls[0].status)
        assertEquals("read_file", last.toolCalls[0].name)
    }

    @Test
    fun `ToolCallStarted marks tool as running`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(
                id = "a1",
                toolCalls = persistentListOf(ToolCallEntry(callId = "t1", name = "read_file", arguments = "{}"))
            )),
        )
        val event = ChatEvent.ToolCallStarted(id = "t1", name = "read_file", arguments = "{}")
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        assertEquals(ToolStatus.Running, (newState.messages.last() as Message.Assistant).toolCalls[0].status)
    }

    @Test
    fun `ToolOutputChunk routes to correct tool by callId`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(
                id = "a1",
                toolCalls = persistentListOf(
                    ToolCallEntry(callId = "t1", name = "read_file", arguments = "{}", status = ToolStatus.Running),
                    ToolCallEntry(callId = "t2", name = "bash", arguments = "{}", status = ToolStatus.Running),
                )
            )),
        )
        // Output for t2 should go to t2, not t1
        val event = ChatEvent.ToolOutputChunk(id = "t2", chunk = "output")
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        val tools = (newState.messages.last() as Message.Assistant).toolCalls
        assertEquals("", tools[0].output)  // t1 unchanged
        assertEquals("output", tools[1].output)  // t2 got output
    }

    @Test
    fun `ToolCallResult sets output and success`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(
                id = "a1",
                toolCalls = persistentListOf(ToolCallEntry(callId = "t1", name = "read_file", arguments = "{}", status = ToolStatus.Running))
            )),
        )
        val event = ChatEvent.ToolCallResult(id = "t1", name = "read_file", output = "content", success = true, duration_ms = 150)
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        val tc = (newState.messages.last() as Message.Assistant).toolCalls[0]
        assertEquals(ToolStatus.Success, tc.status)
        assertEquals("content", tc.output)
        assertEquals(150L, tc.durationMs)
    }

    @Test
    fun `Artifact events accumulate content`() {
        var state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(id = "a1")),
        )
        state = reduce(state, ChatAction.DaemonEvent(
            ChatEvent.ArtifactStart(id = "a1", artifact_type = "code", language = "kt")
        ), ids)
        assertEquals(1, (state.messages.last() as Message.Assistant).artifacts.size)

        state = reduce(state, ChatAction.DaemonEvent(
            ChatEvent.ArtifactContent(id = "a1", content = "fun main()")
        ), ids)
        assertEquals("fun main()", (state.messages.last() as Message.Assistant).artifacts[0].content)

        state = reduce(state, ChatAction.DaemonEvent(ChatEvent.ArtifactEnd(id = "a1")), ids)
        assertEquals(ArtifactStatus.Complete, (state.messages.last() as Message.Assistant).artifacts[0].status)
    }

    @Test
    fun `PermissionRequest pauses generation`() {
        val state = ChatState(tabId = "t1", generation = GenerationState.Streaming("a1"))
        val event = ChatEvent.PermissionRequest(
            session_id = "s1", tool_name = "bash", reason = "run", call_id = "c1", arguments = "{}"
        )
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        assertNotNull(newState.pendingPermission)
        assertEquals("bash", newState.pendingPermission!!.toolName)
        assertTrue(newState.generation is GenerationState.WaitingPermission)
    }

    @Test
    fun `PermissionDecision resumes streaming`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.WaitingPermission("c1"),
            pendingPermission = PermissionRequestState("s1", "bash", "run", "c1", "{}"),
        )
        val newState = reduce(state, ChatAction.PermissionDecision("c1", PermissionDecisionKind.Allow), ids)
        assertNull(newState.pendingPermission)
        assertTrue(newState.generation is GenerationState.Streaming)
    }

    @Test
    fun `Done completes generation and preserves token data`() {
        val state = ChatState(
            tabId = "t1",
            generation = GenerationState.Streaming("a1"),
            messages = persistentListOf(Message.Assistant(id = "a1", markdown = "done")),
            tokens = TokenUsageState(prompt = 100, completion = 50, total = 150),
        )
        val event = ChatEvent.Done(tokens = 200, tool_calls = 2, session_id = "s123")
        val newState = reduce(state, ChatAction.DaemonEvent(event), ids)
        assertTrue(newState.generation is GenerationState.Idle)
        assertEquals("s123", newState.sessionId)
        assertEquals(AssistantStatus.Complete, (newState.messages.last() as Message.Assistant).status)
        // 保留已有的 prompt/completion，仅更新 total
        assertEquals(100, newState.tokens!!.prompt)
        assertEquals(50, newState.tokens!!.completion)
        assertEquals(200, newState.tokens!!.total)
    }

    @Test
    fun `Stopped resets to Idle`() {
        val state = ChatState(tabId = "t1", generation = GenerationState.Stopping)
        val newState = reduce(state, ChatAction.DaemonEvent(ChatEvent.Stopped), ids)
        assertTrue(newState.generation is GenerationState.Idle)
    }

    @Test
    fun `Error transitions to Failed`() {
        val state = ChatState(tabId = "t1", generation = GenerationState.Streaming("a1"))
        val newState = reduce(state, ChatAction.DaemonEvent(ChatEvent.Error(message = "connection lost")), ids)
        assertTrue(newState.generation is GenerationState.Failed)
        assertEquals("connection lost", (newState.generation as GenerationState.Failed).message)
    }
}
