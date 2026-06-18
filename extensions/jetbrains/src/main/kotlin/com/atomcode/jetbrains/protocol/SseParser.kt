package com.atomcode.jetbrains.protocol

/**
 * SSE (Server-Sent Events) 行解析器。
 * 缓冲输入行，按 \n\n 拆分事件，解析 data: 行中的 JSON payload 为 ChatEvent。
 *
 * 线程安全：所有输入通过 feed()/flush() 串行处理，无内部锁。
 * 缓冲区上限 10MB，超出后返回 Error 事件并清空缓冲区。
 */
class SseParser(
    private val maxBufferSize: Int = 10 * 1024 * 1024 // 10MB
) {
    private val buffer = StringBuilder()

    /**
     * 喂入一段字符串数据。返回此段中解析出的所有完整 SSE 事件。
     * 不完整的事件保留在缓冲区中，等待后续 feed 或 flush。
     */
    fun feed(chunk: String): List<ChatEvent> {
        buffer.append(chunk)

        if (buffer.length > maxBufferSize) {
            buffer.clear()
            return listOf(ChatEvent.Error("SSE buffer exceeded $maxBufferSize bytes limit"))
        }

        val events = mutableListOf<ChatEvent>()
        while (true) {
            val boundaryIndex = buffer.indexOf("\n\n")
            if (boundaryIndex == -1) break

            val eventBlock = buffer.substring(0, boundaryIndex)
            buffer.delete(0, boundaryIndex + 2)

            val event = parseEventBlock(eventBlock)
            if (event != null) {
                events.add(event)
            }
        }
        return events
    }

    /**
     * 清空缓冲区，解析其中剩余数据为事件。
     * 在 SSE 流结束后调用。
     */
    fun flush(): List<ChatEvent> {
        if (buffer.isEmpty()) return emptyList()
        val remaining = buffer.toString().trim()
        buffer.clear()
        if (remaining.isEmpty()) return emptyList()
        val event = parseEventBlock(remaining)
        return if (event != null) listOf(event) else emptyList()
    }

    private fun parseEventBlock(block: String): ChatEvent? {
        val dataLines = mutableListOf<String>()
        for (line in block.lines()) {
            val trimmed = line.trim()
            if (trimmed.isEmpty()) continue
            if (trimmed.startsWith(":")) continue       // SSE 注释行
            if (trimmed.startsWith("data:")) {
                dataLines.add(trimmed.removePrefix("data:").trim())
            }
            // 忽略其他字段（event:, id:, retry:）
        }
        if (dataLines.isEmpty()) return null

        val payload = dataLines.joinToString("\n")
        if (payload.isBlank()) return null

        return deserializeChatEvent(payload)
    }
}
