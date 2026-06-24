package com.atomcode.jetbrains.daemon

import com.google.gson.JsonArray
import com.google.gson.JsonElement
import com.google.gson.JsonNull
import com.google.gson.JsonObject
import com.google.gson.JsonParser

class SseParser {
    private var buffer = StringBuilder()
    private val maxBufferSize = 10 * 1024 * 1024 // 10 MB

    fun feed(chunk: String): List<ChatEvent> {
        buffer.append(chunk)
        if (buffer.length > maxBufferSize) {
            val error = listOf(ChatEvent.Error("SSE buffer exceeded ${maxBufferSize / 1024 / 1024} MB limit"))
            buffer.clear()
            return error
        }
        val events = mutableListOf<ChatEvent>()

        while (true) {
            val marker = buffer.indexOf("\n\n")
            if (marker < 0) break
            val rawEvent = buffer.substring(0, marker)
            buffer.delete(0, marker + 2)
            parseEvent(rawEvent)?.let(events::add)
        }

        return events
    }

    fun flush(): List<ChatEvent> {
        if (buffer.isBlank()) {
            buffer.clear()
            return emptyList()
        }
        val event = parseEvent(buffer.toString())
        buffer.clear()
        return listOfNotNull(event)
    }

    private fun parseEvent(raw: String): ChatEvent? {
        val data = raw
            .lineSequence()
            .filterNot { it.isBlank() || it.startsWith(":") }
            .filter { it.startsWith("data:") }
            .map { it.removePrefix("data:").trimStart() }
            .joinToString("\n")

        if (data.isBlank()) return null
        val json = data.jsonObjectOrNull() ?: return ChatEvent.Unknown("invalid_json")
        val type = json.string("type") ?: return ChatEvent.Unknown("missing")
        return when (type) {
            "text" -> ChatEvent.Text(json.string("content").orEmpty())
            "reasoning" -> ChatEvent.Reasoning(json.string("content").orEmpty())
            "tool_batch" -> ChatEvent.ToolBatch(data)
            "tool_start" -> ChatEvent.ToolStart(json.string("id"), json.string("name").orEmpty(), json.string("arguments").orEmpty())
            "tool_output" -> ChatEvent.ToolOutput(json.string("chunk").orEmpty())
            "tool_result" -> ChatEvent.ToolResult(
                json.string("id"),
                json.string("name").orEmpty(),
                json.string("output").orEmpty(),
                json.boolean("success") ?: false,
                json.long("duration_ms") ?: 0L,
            )
            "artifact_start" -> ChatEvent.ArtifactStart(
                json.string("id").orEmpty(),
                json.string("artifact_type").orEmpty(),
                json.string("language"),
                json.string("title"),
            )
            "artifact_content" -> ChatEvent.ArtifactContent(json.string("id").orEmpty(), json.string("content").orEmpty())
            "artifact_end" -> ChatEvent.ArtifactEnd(json.string("id").orEmpty())
            "permission_request" -> ChatEvent.PermissionRequest(
                json.string("session_id").orEmpty(),
                json.string("tool_name").orEmpty(),
                json.string("reason").orEmpty(),
                json.string("call_id").orEmpty(),
                json.string("arguments").orEmpty(),
            )
            "tokens" -> ChatEvent.Tokens(json.int("prompt") ?: 0, json.int("completion") ?: 0, json.int("total") ?: 0)
            "warning" -> ChatEvent.Warning(json.string("message").orEmpty())
            "done" -> ChatEvent.Done(json.int("tokens") ?: 0, json.int("tool_calls") ?: 0, json.string("session_id"))
            "stopped" -> ChatEvent.Stopped
            "error" -> ChatEvent.Error(json.string("message").orEmpty())
            else -> ChatEvent.Unknown(type)
        }
    }
}

internal fun String.jsonString(key: String): String? = jsonObjectOrNull()?.string(key)

internal fun String.jsonInt(key: String): Int? = jsonObjectOrNull()?.int(key)

internal fun String.jsonLong(key: String): Long? = jsonObjectOrNull()?.long(key)

internal fun String.jsonBoolean(key: String): Boolean? = jsonObjectOrNull()?.boolean(key)

internal fun String.jsonObjects(): List<String> =
    jsonArrayOrNull()
        ?.mapNotNull { it.asObjectOrNull()?.toString() }
        .orEmpty()

internal fun String.jsonArrayObjects(key: String): List<String> =
    jsonObjectOrNull()
        ?.array(key)
        ?.mapNotNull { it.asObjectOrNull()?.toString() }
        .orEmpty()

internal fun String.jsonNestedObject(key: String): String? =
    jsonObjectOrNull()?.objectOrNull(key)?.toString()

private fun String.jsonObjectOrNull(): JsonObject? = parseJsonElementOrNull()?.asObjectOrNull()

private fun String.jsonArrayOrNull(): JsonArray? = parseJsonElementOrNull()?.takeIf { it.isJsonArray }?.asJsonArray

private fun String.parseJsonElementOrNull(): JsonElement? =
    try {
        JsonParser.parseString(this)
    } catch (_: Exception) {
        null
    }

private fun JsonElement.asObjectOrNull(): JsonObject? =
    takeIf { it.isJsonObject }?.asJsonObject

private fun JsonObject.value(key: String): JsonElement? =
    get(key)?.takeUnless { it is JsonNull || it.isJsonNull }

private fun JsonObject.string(key: String): String? =
    value(key)?.takeIf { it.isJsonPrimitive }?.asJsonPrimitive?.takeIf { it.isString }?.asString

private fun JsonObject.int(key: String): Int? =
    long(key)?.takeIf { it in Int.MIN_VALUE..Int.MAX_VALUE }?.toInt()

private fun JsonObject.long(key: String): Long? =
    try {
        value(key)?.takeIf { it.isJsonPrimitive }?.asJsonPrimitive?.takeIf { it.isNumber }?.asLong
    } catch (_: NumberFormatException) {
        null
    }

private fun JsonObject.boolean(key: String): Boolean? =
    value(key)?.takeIf { it.isJsonPrimitive }?.asJsonPrimitive?.takeIf { it.isBoolean }?.asBoolean

private fun JsonObject.array(key: String): JsonArray? =
    value(key)?.takeIf { it.isJsonArray }?.asJsonArray

private fun JsonObject.objectOrNull(key: String): JsonObject? =
    value(key)?.asObjectOrNull()
