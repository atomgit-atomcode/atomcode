package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.protocol.ApprovalMode
import com.sun.net.httpserver.HttpServer
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference
import java.util.concurrent.CopyOnWriteArrayList

class ChatStoreTest {
    @Test
    fun `setApprovalMode rolls back optimistic state when daemon update fails`() = runBlocking {
        val latch = CountDownLatch(1)
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.createContext("/approval_mode") { exchange ->
            latch.countDown()
            val body = """{"error":"failed"}""".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(500, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.start()

        try {
            val client = DaemonClient("http://127.0.0.1:${server.address.port}")
            val scope = CoroutineScope(Dispatchers.IO)
            val store = ChatStore("t1", client, scope)
            try {
                store.setApprovalMode(ApprovalMode.Plan)

                assertTrue(latch.await(2, TimeUnit.SECONDS), "daemon should receive mode update")
                withTimeout(2_000) {
                    while (store.state.value.approvalMode != ApprovalMode.Build.wire) {
                        delay(10)
                    }
                }
                assertEquals(ApprovalMode.Build.wire, store.state.value.approvalMode)
            } finally {
                scope.cancel()
            }
        } finally {
            server.stop(0)
        }
    }

    @Test
    fun `submitPrompt waits for pending mode switch and then uses captured confirmed approval mode`() = runBlocking {
        val modeStarted = CountDownLatch(1)
        val releaseMode = CountDownLatch(1)
        val chatReceived = CountDownLatch(1)
        val chatBody = AtomicReference("")
        val executor = Executors.newCachedThreadPool()
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.executor = executor
        server.createContext("/approval_mode") { exchange ->
            modeStarted.countDown()
            releaseMode.await(2, TimeUnit.SECONDS)
            val body = """{"ok":true,"mode":"plan"}""".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.createContext("/chat") { exchange ->
            chatBody.set(exchange.requestBody.bufferedReader(Charsets.UTF_8).readText())
            chatReceived.countDown()
            val body = """data: {"type":"done","tokens":0,"tool_calls":0,"session_id":"s1"}\n\n"""
                .toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "text/event-stream")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.start()

        try {
            val client = DaemonClient("http://127.0.0.1:${server.address.port}")
            val scope = CoroutineScope(Dispatchers.IO)
            val store = ChatStore("t1", client, scope)
            try {
                store.setApprovalMode(ApprovalMode.Plan)
                assertTrue(modeStarted.await(2, TimeUnit.SECONDS), "daemon should receive mode update")

                store.submitPrompt("hello")

                assertEquals(
                    false,
                    chatReceived.await(200, TimeUnit.MILLISECONDS),
                    "chat request should not be sent while approval mode switch is pending",
                )
                releaseMode.countDown()
                assertTrue(chatReceived.await(2, TimeUnit.SECONDS), "daemon should receive chat request")
                assertTrue(
                    chatBody.get().contains(""""approval_mode":"${ApprovalMode.Build.wire}""""),
                    "queued request should use approval mode captured before the pending switch completed",
                )
            } finally {
                releaseMode.countDown()
                scope.cancel()
            }
        } finally {
            server.stop(0)
            executor.shutdownNow()
        }
    }

    @Test
    fun `setApprovalMode ignores a second selection while update is pending`() = runBlocking {
        val firstStarted = CountDownLatch(1)
        val releaseMode = CountDownLatch(1)
        val requests = AtomicInteger(0)
        val executor = Executors.newCachedThreadPool()
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.executor = executor
        server.createContext("/approval_mode") { exchange ->
            requests.incrementAndGet()
            firstStarted.countDown()
            releaseMode.await(2, TimeUnit.SECONDS)
            val body = """{"ok":true,"mode":"plan"}""".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.start()

        try {
            val client = DaemonClient("http://127.0.0.1:${server.address.port}")
            val scope = CoroutineScope(Dispatchers.IO)
            val store = ChatStore("t1", client, scope)
            try {
                store.setApprovalMode(ApprovalMode.Plan)
                assertTrue(firstStarted.await(2, TimeUnit.SECONDS), "daemon should receive first mode update")

                store.setApprovalMode(ApprovalMode.Bypass)
                delay(200)

                assertEquals(1, requests.get())
            } finally {
                releaseMode.countDown()
                scope.cancel()
            }
        } finally {
            server.stop(0)
            executor.shutdownNow()
        }
    }

    @Test
    fun `queued prompt drains with approval mode captured when it was queued`() = runBlocking {
        val firstChatStarted = CountDownLatch(1)
        val releaseFirstChat = CountDownLatch(1)
        val secondChatReceived = CountDownLatch(1)
        val chatRequests = AtomicInteger(0)
        val chatBodies = CopyOnWriteArrayList<String>()
        val executor = Executors.newCachedThreadPool()
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.executor = executor
        server.createContext("/approval_mode") { exchange ->
            val body = """{"ok":true,"mode":"plan"}""".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.createContext("/chat") { exchange ->
            val index = chatRequests.incrementAndGet()
            chatBodies.add(exchange.requestBody.bufferedReader(Charsets.UTF_8).readText())
            if (index == 1) {
                firstChatStarted.countDown()
                releaseFirstChat.await(2, TimeUnit.SECONDS)
            } else {
                secondChatReceived.countDown()
            }
            val body = """data: {"type":"done","tokens":0,"tool_calls":0,"session_id":"s1"}\n\n"""
                .toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "text/event-stream")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
        }
        server.start()

        try {
            val client = DaemonClient("http://127.0.0.1:${server.address.port}")
            val scope = CoroutineScope(Dispatchers.IO)
            val store = ChatStore("t1", client, scope)
            try {
                store.submitPrompt("first")
                assertTrue(firstChatStarted.await(2, TimeUnit.SECONDS), "first chat should start")

                store.submitPrompt(
                    text = "queued",
                    images = listOf(ImageRef("image/png", "abc123")),
                    workingDir = "/tmp/project",
                )
                store.setApprovalMode(ApprovalMode.Plan)
                withTimeout(2_000) {
                    while (store.state.value.confirmedApprovalMode != ApprovalMode.Plan.wire) {
                        delay(10)
                    }
                }

                releaseFirstChat.countDown()

                assertTrue(secondChatReceived.await(2, TimeUnit.SECONDS), "queued chat should drain")
                assertEquals(2, chatBodies.size)
                assertTrue(
                    chatBodies[1].contains(""""approval_mode":"${ApprovalMode.Build.wire}""""),
                    "queued request should use approval mode captured at queue time",
                )
                assertTrue(chatBodies[1].contains(""""media_type":"image/png""""))
                assertTrue(chatBodies[1].contains(""""data":"abc123""""))
                assertTrue(chatBodies[1].contains(""""working_dir":"/tmp/project""""))
            } finally {
                releaseFirstChat.countDown()
                scope.cancel()
            }
        } finally {
            server.stop(0)
            executor.shutdownNow()
        }
    }
}
