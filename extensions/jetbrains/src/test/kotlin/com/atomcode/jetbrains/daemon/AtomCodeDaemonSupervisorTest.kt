package com.atomcode.jetbrains.daemon

import com.atomcode.jetbrains.settings.AtomCodeSettings
import com.atomcode.jetbrains.services.DaemonConnectionException
import com.atomcode.jetbrains.services.DaemonControl
import com.atomcode.jetbrains.services.DaemonControlFactory
import com.atomcode.jetbrains.services.DaemonProcessFactory
import com.atomcode.jetbrains.services.DaemonSupervisorEngine
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertSame
import kotlin.test.assertTrue

class AtomCodeDaemonSupervisorTest {
    private val settings = AtomCodeSettings(host = "127.0.0.1", port = 13456)
    private val auth = DaemonAuth(null)

    @Test
    fun `concurrent callers share one daemon start`() {
        val healthCalls = AtomicInteger()
        val starts = AtomicInteger()
        val ready = CompletableFuture<HealthResponse>()
        val engine = engine(
            health = {
                if (healthCalls.incrementAndGet() == 1) failed("connection refused") else ready
            },
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(DaemonLaunchResult.Started(FakeProcessHandle()))
            },
        )

        val first = engine.ensureReady(settings, auth)
        val second = engine.ensureReady(settings, auth)

        assertSame(first, second)
        assertEquals(1, starts.get())
        ready.complete(health())
        assertEquals("1.2.3", first.get(1, TimeUnit.SECONDS).version)
    }

    @Test
    fun `process exit fails immediately instead of waiting for startup timeout`() {
        val process = FakeProcessHandle(
            CompletableFuture.completedFuture(DaemonProcessExit(23, "bind failed")),
        )
        val engine = engine(
            health = { failed("connection refused") },
            start = { CompletableFuture.completedFuture(DaemonLaunchResult.Started(process)) },
        )

        val error = runCatching {
            engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        val cause = error.cause
        assertTrue(cause is DaemonConnectionException)
        assertEquals(ConnectionErrorKind.StartFailed, cause.kind)
        assertTrue(cause.message.orEmpty().contains("bind failed"))
    }

    @Test
    fun `unexpected service on configured port does not start daemon`() {
        val starts = AtomicInteger()
        val engine = engine(
            health = { CompletableFuture.completedFuture(health(service = "other-service")) },
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(DaemonLaunchResult.Started(FakeProcessHandle()))
            },
        )

        val error = runCatching {
            engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        val cause = error.cause
        assertTrue(cause is DaemonConnectionException)
        assertEquals(ConnectionErrorKind.PortUsedByNonAtomCode, cause.kind)
        assertEquals(0, starts.get())
    }

    @Test
    fun `matching running daemon is reused without starting a process`() {
        val starts = AtomicInteger()
        val engine = engine(
            health = { CompletableFuture.completedFuture(health()) },
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(DaemonLaunchResult.Started(FakeProcessHandle()))
            },
            expectedVersion = "1.2.3",
        )

        val ready = engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)

        assertEquals("1.2.3", ready.version)
        assertEquals(0, starts.get())
    }

    @Test
    fun `version mismatch stops the old daemon and starts the bundled version`() {
        val healthCalls = AtomicInteger()
        val starts = AtomicInteger()
        val shutdowns = AtomicInteger()
        val engine = engine(
            health = {
                when (healthCalls.incrementAndGet()) {
                    1 -> CompletableFuture.completedFuture(health(version = "1.0.0"))
                    2 -> failed("daemon stopped")
                    else -> CompletableFuture.completedFuture(health(version = "1.2.3"))
                }
            },
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(DaemonLaunchResult.Started(FakeProcessHandle()))
            },
            expectedVersion = "1.2.3",
            shutdown = {
                shutdowns.incrementAndGet()
                CompletableFuture.completedFuture(true)
            },
        )

        val ready = engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)

        assertEquals("1.2.3", ready.version)
        assertEquals(1, shutdowns.get())
        assertEquals(1, starts.get())
    }

    @Test
    fun `missing daemon binary is a terminal setup failure`() {
        val engine = engine(
            health = { failed("connection refused") },
            start = { CompletableFuture.completedFuture(DaemonLaunchResult.MissingBinary) },
        )

        val error = runCatching {
            engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        val cause = error.cause
        assertTrue(cause is DaemonConnectionException)
        assertEquals(ConnectionErrorKind.MissingBinary, cause.kind)
    }

    @Test
    fun `transient health failure reuses the owned daemon instead of starting another`() {
        val healthCalls = AtomicInteger()
        val starts = AtomicInteger()
        val process = FakeProcessHandle()
        val engine = engine(
            health = {
                when (healthCalls.incrementAndGet()) {
                    1, 3 -> failed("connection refused")
                    else -> CompletableFuture.completedFuture(health())
                }
            },
            start = {
                starts.incrementAndGet()
                CompletableFuture.completedFuture(DaemonLaunchResult.Started(process))
            },
        )

        engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)

        assertEquals(1, starts.get())
        assertTrue(process.isAlive())
    }

    @Test
    fun `startup timeout destroys the stuck process so a later attempt can restart`() {
        val starts = AtomicInteger()
        val clock = AtomicLong()
        val processes = mutableListOf<FakeProcessHandle>()
        val engine = DaemonSupervisorEngine(
            controlFactory = DaemonControlFactory { _, _, _ ->
                object : DaemonControl {
                    override fun health(): CompletableFuture<HealthResponse> = failed("connection refused")
                    override fun shutdown(): CompletableFuture<Boolean> = CompletableFuture.completedFuture(true)
                }
            },
            processFactory = DaemonProcessFactory {
                object : DaemonProcessLauncher {
                    override fun expectedVersion(): String? = null
                    override fun expectedHash(): String? = null
                    override fun start(): CompletableFuture<DaemonLaunchResult> {
                        starts.incrementAndGet()
                        val process = FakeProcessHandle()
                        processes += process
                        return CompletableFuture.completedFuture(DaemonLaunchResult.Started(process))
                    }
                }
            },
            startupTimeoutNanos = 1,
            retryDelayMs = 1,
            nanoTime = clock::getAndIncrement,
        )

        repeat(2) {
            val error = runCatching {
                engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
            }.exceptionOrNull()
            assertTrue(error is ExecutionException)
            assertEquals(ConnectionErrorKind.Timeout, (error.cause as DaemonConnectionException).kind)
        }

        assertEquals(2, starts.get())
        assertTrue(processes.all { it.destroyed.get() })
    }

    @Test
    fun `dispose destroys a process whose async start completes afterwards`() {
        val launch = CompletableFuture<DaemonLaunchResult>()
        val process = FakeProcessHandle()
        val engine = engine(
            health = { failed("connection refused") },
            start = { launch },
        )

        engine.ensureReady(settings, auth)
        engine.dispose()
        launch.complete(DaemonLaunchResult.Started(process))

        assertTrue(process.destroyed.get())
    }

    @Test
    fun `same endpoint shares one start while binary setting changes`() {
        val starts = AtomicInteger()
        val launch = CompletableFuture<DaemonLaunchResult>()
        val daemonReady = AtomicBoolean(false)
        val engine = engine(
            health = {
                if (daemonReady.get()) CompletableFuture.completedFuture(health())
                else failed("connection refused")
            },
            start = {
                starts.incrementAndGet()
                launch
            },
        )
        val firstSettings = settings.copy(daemonBinaryPath = "/first/atomcode-daemon")
        val secondSettings = settings.copy(daemonBinaryPath = "/second/atomcode-daemon")

        val first = engine.ensureReady(firstSettings, auth)
        val second = engine.ensureReady(secondSettings, auth)

        assertEquals(1, starts.get())
        assertFalse(first.isDone)
        assertFalse(second.isDone)

        daemonReady.set(true)
        launch.complete(DaemonLaunchResult.Started(FakeProcessHandle()))

        first.get(1, TimeUnit.SECONDS)
        second.get(1, TimeUnit.SECONDS)
        assertEquals(1, starts.get())
    }

    @Test
    fun `launched daemon with an unexpected version is rejected and destroyed`() {
        val healthCalls = AtomicInteger()
        val process = FakeProcessHandle()
        val engine = engine(
            health = {
                if (healthCalls.incrementAndGet() == 1) failed("connection refused")
                else CompletableFuture.completedFuture(health(version = "0.9.0"))
            },
            start = { CompletableFuture.completedFuture(DaemonLaunchResult.Started(process)) },
            expectedVersion = "1.2.3",
        )

        val error = runCatching {
            engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        assertEquals(ConnectionErrorKind.IncompatibleDaemon, (error.cause as DaemonConnectionException).kind)
        assertTrue(process.destroyed.get())
    }

    @Test
    fun `unexpected service after launch destroys the owned process`() {
        val healthCalls = AtomicInteger()
        val process = FakeProcessHandle()
        val engine = engine(
            health = {
                if (healthCalls.incrementAndGet() == 1) failed("connection refused")
                else CompletableFuture.completedFuture(health(service = "other-service"))
            },
            start = { CompletableFuture.completedFuture(DaemonLaunchResult.Started(process)) },
        )

        val error = runCatching {
            engine.ensureReady(settings, auth).get(1, TimeUnit.SECONDS)
        }.exceptionOrNull()

        assertTrue(error is ExecutionException)
        assertEquals(ConnectionErrorKind.PortUsedByNonAtomCode, (error.cause as DaemonConnectionException).kind)
        assertTrue(process.destroyed.get())
    }

    @Test
    fun `auto start policy change does not reuse a disabled startup attempt`() {
        val health = CompletableFuture<HealthResponse>()
        val engine = engine(
            health = { health },
            start = { CompletableFuture.completedFuture(DaemonLaunchResult.Started(FakeProcessHandle())) },
        )

        val disabled = engine.ensureReady(settings.copy(autoStart = false), auth)
        val enabled = engine.ensureReady(settings.copy(autoStart = true), auth)

        assertFalse(disabled === enabled)
    }

    private fun engine(
        health: () -> CompletableFuture<HealthResponse>,
        start: () -> CompletableFuture<DaemonLaunchResult>,
        expectedVersion: String? = null,
        expectedHash: String? = null,
        shutdown: () -> CompletableFuture<Boolean> = { CompletableFuture.completedFuture(true) },
    ): DaemonSupervisorEngine = DaemonSupervisorEngine(
        controlFactory = DaemonControlFactory { _, _, _ ->
            object : DaemonControl {
                override fun health(): CompletableFuture<HealthResponse> = health()
                override fun shutdown(): CompletableFuture<Boolean> = shutdown()
            }
        },
        processFactory = DaemonProcessFactory { _ ->
            object : DaemonProcessLauncher {
                override fun expectedVersion(): String? = expectedVersion
                override fun expectedHash(): String? = expectedHash
                override fun start(): CompletableFuture<DaemonLaunchResult> = start()
            }
        },
        startupTimeoutNanos = TimeUnit.SECONDS.toNanos(10),
        retryDelayMs = 1,
    )

    private fun health(
        service: String = "atomcode-daemon",
        version: String = "1.2.3",
    ) = HealthResponse(
        status = "ok",
        version = version,
        service = service,
    )

    private fun <T> failed(message: String): CompletableFuture<T> =
        CompletableFuture.failedFuture(IllegalStateException(message))
}

private class FakeProcessHandle(
    private val exit: CompletableFuture<DaemonProcessExit> = CompletableFuture(),
) : ManagedDaemonProcess {
    val destroyed = AtomicBoolean(false)

    override fun isAlive(): Boolean = !exit.isDone
    override fun onExit(): CompletableFuture<DaemonProcessExit> = exit
    override fun destroy() {
        destroyed.set(true)
        exit.complete(DaemonProcessExit(143, "terminated"))
    }
}
