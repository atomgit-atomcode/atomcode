package com.atomcode.jetbrains.daemon

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
        val type = data.jsonString("type") ?: return ChatEvent.Unknown("missing")
        return when (type) {
            "text" -> ChatEvent.Text(data.jsonString("content").orEmpty())
            "reasoning" -> ChatEvent.Reasoning(data.jsonString("content").orEmpty())
            "tool_batch" -> ChatEvent.ToolBatch(data)
            "tool_start" -> ChatEvent.ToolStart(data.jsonString("id"), data.jsonString("name").orEmpty(), data.jsonString("arguments").orEmpty())
            "tool_output" -> ChatEvent.ToolOutput(data.jsonString("chunk").orEmpty())
            "tool_result" -> ChatEvent.ToolResult(
                data.jsonString("id"),
                data.jsonString("name").orEmpty(),
                data.jsonString("output").orEmpty(),
                data.jsonBoolean("success") ?: false,
                data.jsonLong("duration_ms") ?: 0L,
            )
            "artifact_start" -> ChatEvent.ArtifactStart(
                data.jsonString("id").orEmpty(),
                data.jsonString("artifact_type").orEmpty(),
                data.jsonString("language"),
                data.jsonString("title"),
            )
            "artifact_content" -> ChatEvent.ArtifactContent(data.jsonString("id").orEmpty(), data.jsonString("content").orEmpty())
            "artifact_end" -> ChatEvent.ArtifactEnd(data.jsonString("id").orEmpty())
            "permission_request" -> ChatEvent.PermissionRequest(
                data.jsonString("session_id").orEmpty(),
                data.jsonString("tool_name").orEmpty(),
                data.jsonString("reason").orEmpty(),
                data.jsonString("call_id").orEmpty(),
                data.jsonString("arguments").orEmpty(),
            )
            "tokens" -> ChatEvent.Tokens(data.jsonInt("prompt") ?: 0, data.jsonInt("completion") ?: 0, data.jsonInt("total") ?: 0)
            "done" -> ChatEvent.Done(data.jsonInt("tokens") ?: 0, data.jsonInt("tool_calls") ?: 0, data.jsonString("session_id"))
            "stopped" -> ChatEvent.Stopped
            "error" -> ChatEvent.Error(data.jsonString("message").orEmpty())
            else -> ChatEvent.Unknown(type)
        }
    }
}

internal fun String.jsonString(key: String): String? {
    val pattern = Regex("\"${Regex.escape(key)}\"\\s*:\\s*\"((?:\\\\.|[^\"\\\\])*)\"")
    return pattern.find(this)?.groupValues?.get(1)?.jsonUnescaped()
}

internal fun String.jsonInt(key: String): Int? = jsonLong(key)?.toInt()

internal fun String.jsonLong(key: String): Long? {
    val pattern = Regex("\"${Regex.escape(key)}\"\\s*:\\s*(-?\\d+)")
    return pattern.find(this)?.groupValues?.get(1)?.toLongOrNull()
}

internal fun String.jsonBoolean(key: String): Boolean? {
    val pattern = Regex("\"${Regex.escape(key)}\"\\s*:\\s*(true|false)")
    return pattern.find(this)?.groupValues?.get(1)?.toBooleanStrictOrNull()
}

internal fun String.jsonObjects(): List<String> {
    val trimmed = trim()
    if (!trimmed.startsWith("[") || !trimmed.endsWith("]")) return emptyList()
    return trimmed.jsonObjectRanges(0, trimmed.length).map { trimmed.substring(it.first, it.second) }
}

internal fun String.jsonArrayObjects(key: String): List<String> {
    val keyPattern = Regex("\"${Regex.escape(key)}\"\\s*:\\s*\\[")
    val match = keyPattern.find(this) ?: return emptyList()
    val arrayStart = match.range.last
    var depth = 0
    var inString = false
    var escaped = false

    for (index in arrayStart until length) {
        val char = this[index]
        if (escaped) {
            escaped = false
            continue
        }
        if (char == '\\' && inString) {
            escaped = true
            continue
        }
        if (char == '"') {
            inString = !inString
            continue
        }
        if (inString) continue
        when (char) {
            '[' -> depth++
            ']' -> {
                depth--
                if (depth == 0) {
                    return substring(arrayStart, index + 1).jsonObjects()
                }
            }
        }
    }
    return emptyList()
}

internal fun String.jsonNestedObject(key: String): String? {
    val keyPattern = Regex("\"${Regex.escape(key)}\"\\s*:\\s*\\{")
    val match = keyPattern.find(this) ?: return null
    val objectStart = match.range.last
    return jsonObjectRanges(objectStart, length).firstOrNull()?.let { substring(it.first, it.second) }
}

private fun String.jsonObjectRanges(start: Int, end: Int): List<Pair<Int, Int>> {
    val ranges = mutableListOf<Pair<Int, Int>>()
    var depth = 0
    var objectStart = -1
    var inString = false
    var escaped = false

    for (index in start until end) {
        val char = this[index]
        if (escaped) {
            escaped = false
            continue
        }
        if (char == '\\' && inString) {
            escaped = true
            continue
        }
        if (char == '"') {
            inString = !inString
            continue
        }
        if (inString) continue

        when (char) {
            '{' -> {
                if (depth == 0) objectStart = index
                depth++
            }
            '}' -> {
                depth--
                if (depth == 0 && objectStart >= 0) {
                    ranges += objectStart to index + 1
                    objectStart = -1
                }
            }
        }
    }
    return ranges
}

private fun String.jsonUnescaped(): String {
    val result = StringBuilder(length)
    var index = 0
    while (index < length) {
        val char = this[index]
        if (char != '\\' || index == lastIndex) {
            result.append(char)
            index++
            continue
        }

        when (val escaped = this[index + 1]) {
            '"' -> result.append('"')
            '\\' -> result.append('\\')
            '/' -> result.append('/')
            'b' -> result.append('\b')
            'f' -> result.append('\u000C')
            'n' -> result.append('\n')
            'r' -> result.append('\r')
            't' -> result.append('\t')
            'u' -> {
                val hex = substring(index + 2, (index + 6).coerceAtMost(length))
                val code = hex.takeIf { it.length == 4 }?.toIntOrNull(16)
                if (code != null) {
                    result.append(code.toChar())
                    index += 4
                } else {
                    result.append("\\u")
                }
            }
            else -> result.append(escaped)
        }
        index += 2
    }
    return result.toString()
}
