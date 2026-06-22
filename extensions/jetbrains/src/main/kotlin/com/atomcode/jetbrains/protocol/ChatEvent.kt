package com.atomcode.jetbrains.protocol

import com.google.gson.Gson
import com.google.gson.JsonParser
import com.google.gson.reflect.TypeToken

/**
 * daemon /chat SSE 事件类型完整定义。
 * 类名和字段名严格匹配 daemon Rust 端的 serde JSON 输出。
 */
sealed interface ChatEvent {
    data class Text(val content: String) : ChatEvent
    data class Reasoning(val content: String) : ChatEvent
    data class ToolBatchStarted(val calls: List<ToolBatchCall>) : ChatEvent
    data class ToolCallStarted(val id: String, val name: String, val arguments: String) : ChatEvent
    data class ToolOutputChunk(val id: String, val chunk: String) : ChatEvent
    data class ToolCallResult(
        val id: String,
        val name: String,
        val output: String,
        val success: Boolean,
        val duration_ms: Long
    ) : ChatEvent
    data class ArtifactStart(
        val id: String,
        val artifact_type: String,
        val language: String? = null,
        val title: String? = null
    ) : ChatEvent
    data class ArtifactContent(val id: String, val content: String) : ChatEvent
    data class ArtifactEnd(val id: String) : ChatEvent
    data class PermissionRequest(
        val session_id: String,
        val tool_name: String,
        val reason: String,
        val call_id: String,
        val arguments: String,
        val severity: PermissionSeverity? = PermissionSeverity.Destructive,  // daemon 可能不发送
        val scope: String? = null,
        val allow_persist: Boolean? = false  // daemon 可能不发送
    ) : ChatEvent
    data class TokenUsage(val prompt: Int, val completion: Int, val total: Int) : ChatEvent
    data class Done(val tokens: Int, val tool_calls: Int, val session_id: String? = null) : ChatEvent
    data object Stopped : ChatEvent
    data class Error(val message: String) : ChatEvent
}

data class ToolBatchCall(
    val id: String,
    val name: String,
    val arguments: String
)

enum class PermissionSeverity { Safe, Destructive, Critical }

// ── Gson 反序列化 ──

private val chatEventGson = Gson()

fun deserializeChatEvent(payload: String): ChatEvent {
    return try {
        val obj = JsonParser.parseString(payload).asJsonObject
        val type = obj.get("type")?.asString
        when (type) {
            "text" -> chatEventGson.fromJson(payload, ChatEvent.Text::class.java)
            "reasoning" -> chatEventGson.fromJson(payload, ChatEvent.Reasoning::class.java)
            "tool_batch" -> chatEventGson.fromJson(payload, ChatEvent.ToolBatchStarted::class.java)
            "tool_start" -> chatEventGson.fromJson(payload, ChatEvent.ToolCallStarted::class.java)
            "tool_output" -> chatEventGson.fromJson(payload, ChatEvent.ToolOutputChunk::class.java)
            "tool_result" -> chatEventGson.fromJson(payload, ChatEvent.ToolCallResult::class.java)
            "artifact_start" -> chatEventGson.fromJson(payload, ChatEvent.ArtifactStart::class.java)
            "artifact_content" -> chatEventGson.fromJson(payload, ChatEvent.ArtifactContent::class.java)
            "artifact_end" -> chatEventGson.fromJson(payload, ChatEvent.ArtifactEnd::class.java)
            "permission_request" -> chatEventGson.fromJson(payload, ChatEvent.PermissionRequest::class.java)
            "tokens" -> chatEventGson.fromJson(payload, ChatEvent.TokenUsage::class.java)
            "done" -> chatEventGson.fromJson(payload, ChatEvent.Done::class.java)
            "stopped" -> ChatEvent.Stopped
            "error" -> chatEventGson.fromJson(payload, ChatEvent.Error::class.java)
            else -> ChatEvent.Error("Unknown event type: $type — payload: ${payload.take(200)}")
        }
    } catch (e: Exception) {
        ChatEvent.Error("Parse error: ${e.message} — payload: ${payload.take(200)}")
    }
}
