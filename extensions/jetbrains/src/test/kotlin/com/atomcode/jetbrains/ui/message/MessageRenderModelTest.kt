package com.atomcode.jetbrains.ui.message

import com.atomcode.jetbrains.session.AssistantStatus
import com.atomcode.jetbrains.session.ChatState
import com.atomcode.jetbrains.session.MessageState
import com.atomcode.jetbrains.session.PermissionRequestState
import com.atomcode.jetbrains.session.SystemLevel
import com.atomcode.jetbrains.session.ToolStatus
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs

class MessageRenderModelTest {
    @Test
    fun `chat state converts messages to render model`() {
        val state = ChatState(
            tabId = "tab-1",
            messages = listOf(
                MessageState.User("u1", 1, "hello", listOf("src/main.kt")),
                MessageState.Assistant("a1", 2, "**hi**", AssistantStatus.Streaming, "thinking"),
                MessageState.ToolCall("t1", 3, "call-1", "bash", "{}", listOf("out"), ToolStatus.Success, 12),
                MessageState.Permission(
                    "p1",
                    4,
                    PermissionRequestState("r1", "s1", "bash", "reason", "call-1", "{}"),
                ),
                MessageState.System("s1", 5, "system", SystemLevel.Warning),
            ),
        )

        val model = state.toRenderModel()

        assertEquals("tab-1", model.tabId)
        assertEquals(5, model.messages.size)
        assertEquals("hello", assertIs<MessageRenderModel.User>(model.messages[0]).text)
        assertEquals("**hi**", assertIs<MessageRenderModel.Assistant>(model.messages[1]).markdown)
        assertEquals("out", assertIs<MessageRenderModel.ToolCall>(model.messages[2]).output)
        assertEquals("r1", assertIs<MessageRenderModel.Permission>(model.messages[3]).requestId)
        assertEquals(SystemLevel.Warning, assertIs<MessageRenderModel.System>(model.messages[4]).level)
    }
}
