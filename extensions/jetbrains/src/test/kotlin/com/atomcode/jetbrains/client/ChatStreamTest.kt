package com.atomcode.jetbrains.client

import com.atomcode.jetbrains.protocol.ChatEvent
import com.sun.net.httpserver.HttpServer
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest

class ChatStreamTest {
    @Test
    fun `http error response emits chat error event`() = runBlocking {
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.createContext("/chat") { exchange ->
            val body = """{"error":"bad request"}""".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(400, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.start()
        try {
            val request = HttpRequest.newBuilder()
                .uri(URI.create("http://127.0.0.1:${server.address.port}/chat"))
                .POST(HttpRequest.BodyPublishers.ofString("{}"))
                .build()

            val events = ChatStream(HttpClient.newHttpClient(), request).events().toList()

            assertEquals(listOf(ChatEvent.Error("Daemon request failed: HTTP 400: bad request")), events)
        } finally {
            server.stop(0)
        }
    }
}
