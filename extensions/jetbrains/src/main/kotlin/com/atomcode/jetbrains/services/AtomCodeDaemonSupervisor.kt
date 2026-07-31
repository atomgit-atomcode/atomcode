package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.AtomCodeDaemonClient
import com.atomcode.jetbrains.daemon.AtomCodeDaemonProcess
import com.atomcode.jetbrains.daemon.ConnectionErrorKind
import com.atomcode.jetbrains.daemon.DaemonAuth
import com.atomcode.jetbrains.daemon.DaemonLaunchResult
import com.atomcode.jetbrains.daemon.DaemonProcessExit
import com.atomcode.jetbrains.daemon.DaemonProcessLauncher
import com.atomcode.jetbrains.daemon.HealthResponse
import com.atomcode.jetbrains.daemon.ManagedDaemonProcess
import com.atomcode.jetbrains.security.SecretRedactor
import com.atomcode.jetbrains.settings.AtomCodeSettings
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.Service
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionException
import java.util.concurrent.TimeUnit

private const val DAEMON_PROBE_TIMEOUT_MS = 3_000
private const val DAEMON_STARTUP_PROBE_TIMEOUT_MS = 1_000
private const val DAEMON_STARTUP_WAIT_SECONDS = 15L
private const val DAEMON_STARTUP_RETRY_DELAY_MS = 150L
private const val DAEMON_STOP_WAIT_SECONDS = 5L

internal data class DaemonEndpointKey(
    val host: String,
    val port: Int,
) {
    companion object {
        fun from(settings: AtomCodeSettings): DaemonEndpointKey = DaemonEndpointKey(
            host = settings.host,
            port = settings.port,
        )
    }
}

internal data class DaemonConnectionKey(
    val endpoint: DaemonEndpointKey,
    val binaryPath: String,
    val autoStart: Boolean,
    val requestTimeoutMs: Int,
) {
    companion object {
        fun from(settings: AtomCodeSettings): DaemonConnectionKey = DaemonConnectionKey(
            endpoint = DaemonEndpointKey.from(settings),
            binaryPath = settings.daemonBinaryPath.trim(),
            autoStart = settings.autoStart,
            requestTimeoutMs = settings.requestTimeoutMs,
        )
    }
}

internal data class DaemonReady(
    val key: DaemonEndpointKey,
    val version: String,
)

internal class DaemonConnectionException(
    val kind: ConnectionErrorKind,
    message: String,
) : IllegalStateException(message)

internal interface DaemonControl {
    fun health(): CompletableFuture<HealthResponse>
    fun shutdown(): CompletableFuture<Boolean>
}

internal fun interface DaemonControlFactory {
    fun create(settings: AtomCodeSettings, timeoutMs: Int, auth: DaemonAuth): DaemonControl
}

internal fun interface DaemonProcessFactory {
    fun create(settings: AtomCodeSettings): DaemonProcessLauncher
}

internal class DaemonSupervisorEngine(
    private val controlFactory: DaemonControlFactory,
    private val processFactory: DaemonProcessFactory,
    private val startupTimeoutNanos: Long = TimeUnit.SECONDS.toNanos(DAEMON_STARTUP_WAIT_SECONDS),
    private val retryDelayMs: Long = DAEMON_STARTUP_RETRY_DELAY_MS,
    private val nanoTime: () -> Long = System::nanoTime,
) : Disposable {
    private val lock = Any()
    private val inFlight = mutableMapOf<DaemonEndpointKey, DaemonOperation>()
    private val ownedProcesses = mutableMapOf<DaemonEndpointKey, ManagedDaemonProcess>()
    private var disposed = false

    fun ensureReady(settings: AtomCodeSettings, auth: DaemonAuth): CompletableFuture<DaemonReady> {
        val snapshot = settings.copy()
        val connectionKey = DaemonConnectionKey.from(snapshot)
        val key = connectionKey.endpoint
        val shared = synchronized(lock) {
            if (disposed) {
                return failed(ConnectionErrorKind.StartFailed, "AtomCode daemon supervisor is disposed.")
            }
            inFlight[key]?.let { operation ->
                return if (operation.connectionKey == connectionKey) {
                    operation.future
                } else {
                    operation.future.handle { _, _ -> Unit }.thenCompose {
                        ensureReady(snapshot, auth)
                    }
                }
            }
            CompletableFuture<DaemonReady>().also {
                inFlight[key] = DaemonOperation(connectionKey, it)
            }
        }

        val operation = try {
            connect(snapshot, auth, key)
        } catch (error: Exception) {
            CompletableFuture.failedFuture(error)
        }
        operation.whenComplete { ready, error ->
            synchronized(lock) {
                if (inFlight[key]?.future === shared) inFlight.remove(key)
            }
            if (error == null) {
                shared.complete(ready)
            } else {
                shared.completeExceptionally(unwrapCompletion(error))
            }
        }
        return shared
    }

    private fun connect(
        settings: AtomCodeSettings,
        auth: DaemonAuth,
        key: DaemonEndpointKey,
    ): CompletableFuture<DaemonReady> {
        val launcher = processFactory.create(settings)
        val expectation = DaemonExpectation(launcher.expectedVersion(), launcher.expectedHash())
        val probe = controlFactory.create(settings, DAEMON_PROBE_TIMEOUT_MS, auth)
        return probe.health()
            .handle { health, error -> HealthAttempt(health, error?.let(::unwrapCompletion)) }
            .thenCompose { attempt ->
                val health = attempt.health
                when {
                    health != null -> acceptOrRestart(settings, auth, key, launcher, expectation, probe, health)
                    !settings.autoStart -> failed(
                        ConnectionErrorKind.StartFailed,
                        buildString {
                            append("AtomCode daemon is not running and auto-start is disabled.")
                            diagnostic(attempt.error)?.let { append(" ").append(it) }
                        },
                    )
                    else -> {
                        val owned = liveOwnedProcess(key)
                        if (owned != null) {
                            val startupControl = controlFactory.create(
                                settings,
                                DAEMON_STARTUP_PROBE_TIMEOUT_MS,
                                auth,
                            )
                            awaitReady(
                                key = key,
                                control = startupControl,
                                process = owned,
                                expectation = expectation,
                                deadlineNanos = nanoTime() + startupTimeoutNanos,
                            )
                        } else {
                            launchAndAwait(settings, auth, key, launcher, expectation)
                        }
                    }
                }
            }
    }

    private fun acceptOrRestart(
        settings: AtomCodeSettings,
        auth: DaemonAuth,
        key: DaemonEndpointKey,
        launcher: DaemonProcessLauncher,
        expectation: DaemonExpectation,
        control: DaemonControl,
        health: HealthResponse,
    ): CompletableFuture<DaemonReady> {
        if (health.service != "atomcode-daemon") {
            return failed(
                ConnectionErrorKind.PortUsedByNonAtomCode,
                "Port ${settings.host}:${settings.port} is used by ${health.service.ifBlank { "another service" }}.",
            )
        }

        if (!expectation.mismatches(health)) {
            return CompletableFuture.completedFuture(DaemonReady(key, health.version))
        }

        return control.shutdown()
            .handle { stopped, _ -> stopped == true }
            .thenCompose {
                waitUntilStopped(control, nanoTime() + TimeUnit.SECONDS.toNanos(DAEMON_STOP_WAIT_SECONDS))
            }
            .thenCompose { stopped ->
                if (!stopped) {
                    failed(
                        ConnectionErrorKind.IncompatibleDaemon,
                        expectation.mismatchMessage(health),
                    )
                } else {
                    launchAndAwait(settings, auth, key, launcher, expectation)
                }
            }
    }

    private fun waitUntilStopped(
        control: DaemonControl,
        deadlineNanos: Long,
    ): CompletableFuture<Boolean> = control.health()
        .handle { _, error -> error != null }
        .thenCompose { stopped ->
            when {
                stopped -> CompletableFuture.completedFuture(true)
                nanoTime() >= deadlineNanos -> CompletableFuture.completedFuture(false)
                else -> delay().thenCompose { waitUntilStopped(control, deadlineNanos) }
            }
        }

    private fun launchAndAwait(
        settings: AtomCodeSettings,
        auth: DaemonAuth,
        key: DaemonEndpointKey,
        launcher: DaemonProcessLauncher,
        expectation: DaemonExpectation,
    ): CompletableFuture<DaemonReady> = launcher.start().thenCompose { result ->
        when (result) {
            DaemonLaunchResult.MissingBinary -> failed(
                ConnectionErrorKind.MissingBinary,
                "AtomCode CLI or bundled daemon was not found.",
            )
            is DaemonLaunchResult.Failed -> failed(
                ConnectionErrorKind.StartFailed,
                "Failed to start AtomCode daemon: ${result.message}",
            )
            is DaemonLaunchResult.Started -> {
                val process = synchronized(lock) {
                    if (disposed) return@synchronized null
                    val previous = ownedProcesses[key]
                    if (previous != null && previous !== result.process && previous.isAlive()) {
                        previous
                    } else {
                        ownedProcesses[key] = result.process
                        result.process
                    }
                }
                if (process !== result.process) result.process.destroy()
                if (process == null) {
                    return@thenCompose failed(
                        ConnectionErrorKind.StartFailed,
                        "AtomCode daemon supervisor was disposed during startup.",
                    )
                }
                val control = controlFactory.create(settings, DAEMON_STARTUP_PROBE_TIMEOUT_MS, auth)
                awaitReady(
                    key = key,
                    control = control,
                    process = process,
                    expectation = expectation,
                    deadlineNanos = nanoTime() + startupTimeoutNanos,
                )
            }
        }
    }

    private fun liveOwnedProcess(key: DaemonEndpointKey): ManagedDaemonProcess? = synchronized(lock) {
        val process = ownedProcesses[key] ?: return@synchronized null
        if (process.isAlive()) {
            process
        } else {
            ownedProcesses.remove(key)
            null
        }
    }

    private fun awaitReady(
        key: DaemonEndpointKey,
        control: DaemonControl,
        process: ManagedDaemonProcess,
        expectation: DaemonExpectation,
        deadlineNanos: Long,
    ): CompletableFuture<DaemonReady> {
        completedExit(process)?.let { return confirmAfterExit(key, control, expectation, it) }

        return control.health()
            .handle { health, error -> HealthAttempt(health, error?.let(::unwrapCompletion)) }
            .thenCompose { attempt ->
                val health = attempt.health
                val exit = completedExit(process)
                when {
                    health != null && health.service == "atomcode-daemon" && !expectation.mismatches(health) ->
                        CompletableFuture.completedFuture(DaemonReady(key, health.version))
                    health != null && health.service == "atomcode-daemon" -> {
                        terminateOwnedProcess(key, process)
                        failed(ConnectionErrorKind.IncompatibleDaemon, expectation.mismatchMessage(health))
                    }
                    health != null -> {
                        terminateOwnedProcess(key, process)
                        failed(
                            ConnectionErrorKind.PortUsedByNonAtomCode,
                            "Port ${key.host}:${key.port} is used by ${health.service.ifBlank { "another service" }}.",
                        )
                    }
                    exit != null -> confirmAfterExit(
                        key,
                        control,
                        expectation,
                        exit,
                    )
                    nanoTime() >= deadlineNanos -> {
                        forgetOwnedProcess(key, process)
                        process.destroy()
                        failed(
                            ConnectionErrorKind.Timeout,
                            buildString {
                                append(
                                    "AtomCode daemon did not become ready within " +
                                        "${TimeUnit.NANOSECONDS.toSeconds(startupTimeoutNanos)} seconds.",
                                )
                                diagnostic(attempt.error)?.let { append(" ").append(it) }
                            },
                        )
                    }
                    else -> delay().thenCompose {
                        awaitReady(key, control, process, expectation, deadlineNanos)
                    }
                }
            }
    }

    private fun confirmAfterExit(
        key: DaemonEndpointKey,
        control: DaemonControl,
        expectation: DaemonExpectation,
        exit: DaemonProcessExit,
    ): CompletableFuture<DaemonReady> {
        synchronized(lock) {
            ownedProcesses[key]?.takeUnless(ManagedDaemonProcess::isAlive)?.let {
                ownedProcesses.remove(key)
            }
        }
        return control.health()
            .handle { health, _ -> health }
            .thenCompose { health ->
                if (health?.service == "atomcode-daemon" && !expectation.mismatches(health)) {
                    CompletableFuture.completedFuture(DaemonReady(key, health.version))
                } else if (health?.service == "atomcode-daemon") {
                    failed(ConnectionErrorKind.IncompatibleDaemon, expectation.mismatchMessage(health))
                } else {
                    val detail = SecretRedactor.redact(exit.stderr).ifBlank { "no daemon diagnostics" }
                    failed(
                        ConnectionErrorKind.StartFailed,
                        "AtomCode daemon exited with code ${exit.exitCode}: $detail",
                    )
                }
            }
    }

    private fun forgetOwnedProcess(key: DaemonEndpointKey, process: ManagedDaemonProcess) {
        synchronized(lock) {
            if (ownedProcesses[key] === process) ownedProcesses.remove(key)
        }
    }

    private fun terminateOwnedProcess(key: DaemonEndpointKey, process: ManagedDaemonProcess) {
        forgetOwnedProcess(key, process)
        process.destroy()
    }

    private fun completedExit(process: ManagedDaemonProcess): DaemonProcessExit? =
        if (process.onExit().isDone && !process.onExit().isCompletedExceptionally) {
            process.onExit().getNow(null)
        } else {
            null
        }

    private fun delay(): CompletableFuture<Unit> = CompletableFuture.supplyAsync(
        { Unit },
        CompletableFuture.delayedExecutor(retryDelayMs.coerceAtLeast(1), TimeUnit.MILLISECONDS),
    )

    override fun dispose() {
        val processes = synchronized(lock) {
            disposed = true
            inFlight.values.forEach { it.future.cancel(true) }
            inFlight.clear()
            ownedProcesses.values.toList().also { ownedProcesses.clear() }
        }
        processes.forEach(ManagedDaemonProcess::destroy)
    }
}

private data class DaemonOperation(
    val connectionKey: DaemonConnectionKey,
    val future: CompletableFuture<DaemonReady>,
)

private data class DaemonExpectation(
    val version: String?,
    val hash: String?,
) {
    fun mismatches(health: HealthResponse): Boolean =
        (hash != null && health.binaryHash != hash) ||
            (version != null && health.version != version)

    fun mismatchMessage(health: HealthResponse): String =
        if (version != null && health.version != version) {
            "AtomCode daemon version mismatch: running ${health.version}, expected $version."
        } else {
            "AtomCode daemon binary does not match the bundled build."
        }
}

@Service(Service.Level.APP)
class AtomCodeDaemonSupervisor : Disposable {
    private val engine = DaemonSupervisorEngine(
        controlFactory = DaemonControlFactory { settings, timeoutMs, auth ->
            val client = AtomCodeDaemonClient(settings.host, settings.port, timeoutMs, auth)
            object : DaemonControl {
                override fun health(): CompletableFuture<HealthResponse> = client.health()
                override fun shutdown(): CompletableFuture<Boolean> = client.shutdown()
            }
        },
        processFactory = DaemonProcessFactory(::AtomCodeDaemonProcess),
    )

    internal fun ensureReady(settings: AtomCodeSettings, auth: DaemonAuth): CompletableFuture<DaemonReady> =
        engine.ensureReady(settings, auth)

    override fun dispose() = engine.dispose()

    companion object {
        fun getInstance(): AtomCodeDaemonSupervisor =
            ApplicationManager.getApplication().getService(AtomCodeDaemonSupervisor::class.java)
    }
}

private data class HealthAttempt(
    val health: HealthResponse?,
    val error: Throwable?,
)

private fun <T> failed(kind: ConnectionErrorKind, message: String): CompletableFuture<T> =
    CompletableFuture.failedFuture(DaemonConnectionException(kind, message))

private fun unwrapCompletion(error: Throwable): Throwable {
    var current = error
    while (current is CompletionException && current.cause != null) {
        current = requireNotNull(current.cause)
    }
    return current
}

private fun diagnostic(error: Throwable?): String? = error?.message
    ?.takeIf(String::isNotBlank)
    ?.let(SecretRedactor::redact)
