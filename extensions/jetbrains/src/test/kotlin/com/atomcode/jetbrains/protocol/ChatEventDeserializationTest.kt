package com.atomcode.jetbrains.protocol

import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test

class ChatEventDeserializationTest {

    @Test
    fun `deserialize text event`() {
        val input = """{"type":"text","content":"Hello World"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.Text)
        assertEquals("Hello World", (event as ChatEvent.Text).content)
    }

    @Test
    fun `deserialize reasoning event`() {
        val input = """{"type":"reasoning","content":"Let me think..."}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.Reasoning)
        assertEquals("Let me think...", (event as ChatEvent.Reasoning).content)
    }

    @Test
    fun `deserialize tool_batch event`() {
        val input = """{"type":"tool_batch","calls":[{"id":"1","name":"read_file","arguments":"{}"}]}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ToolBatchStarted)
        val batch = event as ChatEvent.ToolBatchStarted
        assertEquals(1, batch.calls.size)
        assertEquals("read_file", batch.calls[0].name)
    }

    @Test
    fun `deserialize tool_start event`() {
        val input = """{"type":"tool_start","id":"abc","name":"bash","arguments":"{\"command\":\"ls\"}"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ToolCallStarted)
        assertEquals("abc", (event as ChatEvent.ToolCallStarted).id)
        assertEquals("bash", event.name)
    }

    @Test
    fun `deserialize tool_output event`() {
        val input = """{"type":"tool_output","id":"t1","chunk":"output line 1\n"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ToolOutputChunk)
        assertEquals("t1", (event as ChatEvent.ToolOutputChunk).id)
        assertEquals("output line 1\n", (event as ChatEvent.ToolOutputChunk).chunk)
    }

    @Test
    fun `deserialize tool_result event`() {
        val input = """{"type":"tool_result","id":"abc","name":"read_file","output":"file contents","success":true,"duration_ms":150}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ToolCallResult)
        val result = event as ChatEvent.ToolCallResult
        assertEquals("abc", result.id)
        assertEquals("read_file", result.name)
        assertEquals("file contents", result.output)
        assertTrue(result.success)
        assertEquals(150L, result.duration_ms)
    }

    @Test
    fun `deserialize artifact_start event`() {
        val input = """{"type":"artifact_start","id":"a1","artifact_type":"code","language":"kotlin","title":"Main.kt"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ArtifactStart)
        assertEquals("a1", (event as ChatEvent.ArtifactStart).id)
        assertEquals("code", event.artifact_type)
        assertEquals("kotlin", event.language)
    }

    @Test
    fun `deserialize artifact_content event`() {
        val input = """{"type":"artifact_content","id":"a1","content":"fun main() {}"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ArtifactContent)
        assertEquals("fun main() {}", (event as ChatEvent.ArtifactContent).content)
    }

    @Test
    fun `deserialize artifact_end event`() {
        val input = """{"type":"artifact_end","id":"a1"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.ArtifactEnd)
        assertEquals("a1", (event as ChatEvent.ArtifactEnd).id)
    }

    @Test
    fun `deserialize permission_request event`() {
        val input = """{"type":"permission_request","session_id":"s1","tool_name":"bash","reason":"executes command","call_id":"c1","arguments":"{\"command\":\"rm -rf /\"}"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.PermissionRequest)
        assertEquals("s1", (event as ChatEvent.PermissionRequest).session_id)
        assertEquals("bash", event.tool_name)
        // daemon 尚未发送 severity/scope/allow_persist，验证默认值
        // Gson 省略缺失字段时 severity 为 null（daemon 尚未发送），外部应 fallback 到 Destructive
        assertNull(event.severity)
        assertNull(event.scope)
        assertNull(event.allow_persist)
    }

    @Test
    fun `deserialize tokens event`() {
        val input = """{"type":"tokens","prompt":100,"completion":50,"total":150}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.TokenUsage)
        assertEquals(100, (event as ChatEvent.TokenUsage).prompt)
        assertEquals(150, event.total)
    }

    @Test
    fun `deserialize done event`() {
        val input = """{"type":"done","tokens":150,"tool_calls":5,"session_id":"abc123"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.Done)
        assertEquals("abc123", (event as ChatEvent.Done).session_id)
        assertEquals(5, event.tool_calls)
    }

    @Test
    fun `deserialize stopped event`() {
        val input = """{"type":"stopped"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.Stopped)
    }

    @Test
    fun `deserialize error event`() {
        val input = """{"type":"error","message":"something went wrong"}"""
        val event = deserializeChatEvent(input)
        assertTrue(event is ChatEvent.Error)
        assertEquals("something went wrong", (event as ChatEvent.Error).message)
    }

    @Test
    fun `unknown event type deserializes without crash`() {
        val input = """{"type":"future_event_type","data":"some value"}"""
        // 不应抛异常——未知类型 + ignoreUnknownKeys 安全跳过
        val event = deserializeChatEvent(input)
        assertNotNull(event)
    }
}
