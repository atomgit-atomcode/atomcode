package com.atomcode.jetbrains.protocol

import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test

class SseParserTest {
    private val parser = SseParser()

    @Test
    fun `parse single text event`() {
        val events = parser.feed("data: {\"type\":\"text\",\"content\":\"hello\"}\n\n")
        assertEquals(1, events.size)
        assertTrue(events[0] is ChatEvent.Text)
        assertEquals("hello", (events[0] as ChatEvent.Text).content)
    }

    @Test
    fun `parse multiple events in one feed`() {
        val input = "data: {\"type\":\"text\",\"content\":\"first\"}\n\ndata: {\"type\":\"text\",\"content\":\"second\"}\n\n"
        val events = parser.feed(input)
        assertEquals(2, events.size)
        assertEquals("first", (events[0] as ChatEvent.Text).content)
        assertEquals("second", (events[1] as ChatEvent.Text).content)
    }

    @Test
    fun `parse events split across multiple feeds`() {
        val events1 = parser.feed("data: {\"type\":\"text\",\"content\":\"hello\"}\n")
        assertEquals(0, events1.size)

        val events2 = parser.feed("\n")
        assertEquals(1, events2.size)
        assertEquals("hello", (events2[0] as ChatEvent.Text).content)
    }

    @Test
    fun `skip comment lines`() {
        val events = parser.feed(": this is a comment\ndata: {\"type\":\"text\",\"content\":\"hello\"}\n\n")
        assertEquals(1, events.size)
        assertTrue(events[0] is ChatEvent.Text)
    }

    @Test
    fun `flush residual buffer`() {
        parser.feed("data: {\"type\":\"text\",\"content\":\"partial\"}\n")
        val events = parser.flush()
        assertEquals(1, events.size)
        assertEquals("partial", (events[0] as ChatEvent.Text).content)
    }

    @Test
    fun `buffer overflow returns error event`() {
        val huge = "x".repeat(11 * 1024 * 1024)
        val events = parser.feed(huge)
        assertTrue(events.isNotEmpty())
        assertTrue(events.any { it is ChatEvent.Error })
    }

    @Test
    fun `empty feed returns empty list`() {
        val events = parser.feed("")
        assertTrue(events.isEmpty())
    }

    @Test
    fun `parse all known event types`() {
        val input = """
            data: {"type":"text","content":"hello"}

            data: {"type":"reasoning","content":"thinking..."}

            data: {"type":"tool_batch","calls":[{"id":"1","name":"read","arguments":"{}"}]}

            data: {"type":"tool_start","id":"1","name":"read","arguments":"{}"}

            data: {"type":"tool_output","id":"1","chunk":"output"}

            data: {"type":"tool_result","id":"1","name":"read","output":"done","success":true,"duration_ms":100}

            data: {"type":"artifact_start","id":"a1","artifact_type":"code","language":"kt"}

            data: {"type":"artifact_content","id":"a1","content":"code"}

            data: {"type":"artifact_end","id":"a1"}

            data: {"type":"permission_request","session_id":"s1","tool_name":"bash","reason":"run","call_id":"c1","arguments":"{}"}

            data: {"type":"tokens","prompt":10,"completion":5,"total":15}

            data: {"type":"done","tokens":15,"tool_calls":1}

            data: {"type":"stopped"}

            data: {"type":"error","message":"fail"}

        """.trimIndent() + "\n"
        val events = parser.feed(input)
        assertEquals(14, events.size)
        assertTrue(events[0] is ChatEvent.Text)
        assertTrue(events[1] is ChatEvent.Reasoning)
        assertTrue(events[2] is ChatEvent.ToolBatchStarted)
        assertTrue(events[3] is ChatEvent.ToolCallStarted)
        assertTrue(events[4] is ChatEvent.ToolOutputChunk)
        assertTrue(events[5] is ChatEvent.ToolCallResult)
        assertTrue(events[6] is ChatEvent.ArtifactStart)
        assertTrue(events[7] is ChatEvent.ArtifactContent)
        assertTrue(events[8] is ChatEvent.ArtifactEnd)
        assertTrue(events[9] is ChatEvent.PermissionRequest)
        assertTrue(events[10] is ChatEvent.TokenUsage)
        assertTrue(events[11] is ChatEvent.Done)
        assertTrue(events[12] is ChatEvent.Stopped)
        assertTrue(events[13] is ChatEvent.Error)
    }
}
