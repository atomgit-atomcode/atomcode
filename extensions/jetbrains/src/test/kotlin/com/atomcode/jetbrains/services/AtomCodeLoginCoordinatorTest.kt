package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.LoginPollResponse
import com.atomcode.jetbrains.daemon.LoginStartResponse
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotSame
import kotlin.test.assertSame
import kotlin.test.assertTrue

class AtomCodeLoginCoordinatorTest {
    @Test
    fun `concurrent callers share one application login attempt`() {
        val poll = CompletableFuture<LoginPollResponse>()
        val starts = AtomicInteger()
        val firstStatuses = CopyOnWriteArrayList<String>()
        val secondStatuses = CopyOnWriteArrayList<String>()
        val transport = FakeLoginTransport(
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(startResponse())
            },
            poll = { poll },
        )
        val coordinator = AtomCodeLoginCoordinator()

        val first = coordinator.login(transport, firstStatuses::add)
        val second = coordinator.login(transport, secondStatuses::add)

        assertSame(first, second)
        assertEquals(1, starts.get())
        assertEquals(listOf("Opened browser for AtomGit sign-in."), secondStatuses)
        poll.complete(LoginPollResponse(status = "authorized", userName = "tester"))
        first.get(1, TimeUnit.SECONDS)
        assertTrue(firstStatuses.last().startsWith("Signed in"))
        assertEquals(firstStatuses, secondStatuses)
    }

    @Test
    fun `one status listener failure does not abort the shared login`() {
        val healthyStatuses = CopyOnWriteArrayList<String>()
        val poll = CompletableFuture<LoginPollResponse>()
        val transport = FakeLoginTransport(
            start = { CompletableFuture.completedFuture(startResponse()) },
            poll = { poll },
        )
        val coordinator = AtomCodeLoginCoordinator()

        val first = coordinator.login(transport) { throw IllegalStateException("listener failed") }
        val second = coordinator.login(transport, healthyStatuses::add)
        poll.complete(LoginPollResponse(status = "authorized", userName = "tester"))

        first.get(1, TimeUnit.SECONDS)
        second.get(1, TimeUnit.SECONDS)
        assertTrue(healthyStatuses.last().startsWith("Signed in"))
    }

    @Test
    fun `completed login attempt is not reused`() {
        val starts = AtomicInteger()
        val transport = FakeLoginTransport(
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(startResponse())
            },
            poll = {
                CompletableFuture.completedFuture(
                    LoginPollResponse(status = "authorized", userName = "tester"),
                )
            },
        )
        val coordinator = AtomCodeLoginCoordinator()

        val first = coordinator.login(transport) {}
        first.get(1, TimeUnit.SECONDS)
        val second = coordinator.login(transport) {}
        second.get(1, TimeUnit.SECONDS)

        assertNotSame(first, second)
        assertEquals(2, starts.get())
    }

    @Test
    fun `typed terminal state completes exceptionally instead of authorizing`() {
        val transport = FakeLoginTransport(
            start = { CompletableFuture.completedFuture(startResponse()) },
            poll = {
                CompletableFuture.completedFuture(
                    LoginPollResponse(
                        status = "expired",
                        userName = null,
                        code = "login_session_expired",
                        message = "Login expired; start a new login.",
                    ),
                )
            },
        )

        val error = runCatching {
            AtomCodeLoginCoordinator().login(transport) {}.get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        assertTrue(error.cause?.message.orEmpty().contains("expired"))
    }
}

private class FakeLoginTransport(
    private val start: () -> CompletableFuture<LoginStartResponse>,
    private val poll: (String) -> CompletableFuture<LoginPollResponse>,
) : LoginTransport {
    override val key: String = "http://daemon"
    override fun start(): CompletableFuture<LoginStartResponse> = start.invoke()
    override fun poll(loginId: String): CompletableFuture<LoginPollResponse> = poll.invoke(loginId)
    override fun cancel(loginId: String): CompletableFuture<Boolean> = CompletableFuture.completedFuture(true)
}

private fun startResponse() = LoginStartResponse(
    loginId = "00000000-0000-0000-0000-000000000001",
    url = "https://example.invalid/login",
    expiresInSeconds = 600,
    daemonInstanceId = "daemon-instance",
)
