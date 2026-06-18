package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.HealthResponse
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class AtomCodeProjectServiceTest {
    @Test
    fun `waitForDaemonHealth retries until daemon reports ready`() {
        val attempts = AtomicInteger(0)
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(1)

        val version = waitForDaemonHealth(deadline, retryDelayMs = 1) {
            when (attempts.incrementAndGet()) {
                1, 2 -> CompletableFuture.failedFuture(IllegalStateException("connection refused"))
                else -> CompletableFuture.completedFuture(
                    HealthResponse(
                        status = "ok",
                        version = "1.2.3",
                        service = "atomcode-daemon",
                    ),
                )
            }
        }.get(1, TimeUnit.SECONDS)

        assertEquals("1.2.3", version)
        assertEquals(3, attempts.get())
    }

    @Test
    fun `waitForDaemonHealth returns null after deadline`() {
        val attempts = AtomicInteger(0)

        val version = waitForDaemonHealth(System.nanoTime(), retryDelayMs = 1) {
            attempts.incrementAndGet()
            CompletableFuture.failedFuture(IllegalStateException("connection refused"))
        }.get(1, TimeUnit.SECONDS)

        assertNull(version)
        assertEquals(1, attempts.get())
    }
}
