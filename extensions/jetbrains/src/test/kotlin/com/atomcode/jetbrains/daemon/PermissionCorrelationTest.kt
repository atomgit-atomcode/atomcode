package com.atomcode.jetbrains.daemon

import com.google.gson.JsonParser
import com.sun.net.httpserver.HttpServer
import java.net.InetSocketAddress
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class PermissionCorrelationTest {
    @Test
    fun `SSE approval identity is echoed unchanged by the HTTP client`() {
        val event = SseParser().feed("data: " + """{"type":"permission_request","session_id":"s","tool_name":"mcp__browser-use__browser_click","call_id":"call","approval_id":"approval","arguments":"{}"}""" + "\n\n").single() as ChatEvent.PermissionRequest
        assertEquals("approval", event.approvalId)
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        val received = AtomicReference("")
        server.createContext("/chat/permission") { exchange ->
            received.set(exchange.requestBody.bufferedReader().readText())
            val bytes = """{"success":true}""".toByteArray()
            exchange.sendResponseHeaders(200, bytes.size.toLong())
            exchange.responseBody.use { it.write(bytes) }
        }
        server.start()
        try {
            val client = AtomCodeDaemonClient("127.0.0.1", server.address.port, 5000)
            assertTrue(client.sendPermissionDecision(event.sessionId, "allow", event.callId, event.approvalId, event.toolName).get(5, TimeUnit.SECONDS).success)
            val body = JsonParser.parseString(received.get()).asJsonObject
            assertEquals("call", body.get("call_id").asString)
            assertEquals("approval", body.get("approval_id").asString)
            assertEquals("s", body.get("session_id").asString)
        } finally {
            server.stop(0)
        }
    }
}
