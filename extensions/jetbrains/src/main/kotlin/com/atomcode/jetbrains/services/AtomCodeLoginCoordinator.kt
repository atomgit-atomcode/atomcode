package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.AtomCodeDaemonClient
import com.atomcode.jetbrains.daemon.DaemonHttpException
import com.atomcode.jetbrains.daemon.LoginPollResponse
import com.atomcode.jetbrains.daemon.LoginStartResponse
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.Service
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

private const val DEFAULT_LOGIN_POLL_DELAY_MS = 2_000L

/** Application-wide single-flight owner for daemon OAuth attempts. */
@Service(Service.Level.APP)
class AtomCodeLoginCoordinator {
    private val attempts = ConcurrentHashMap<String, CompletableFuture<Unit>>()

    @Synchronized
    fun login(
        client: AtomCodeDaemonClient,
        onStatus: (String) -> Unit,
    ): CompletableFuture<Unit> = login(
        object : LoginTransport {
            override val key: String = client.loginCoordinatorKey()
            override fun start(): CompletableFuture<LoginStartResponse> = client.startLogin(true)
            override fun poll(loginId: String): CompletableFuture<LoginPollResponse> = client.pollLogin(loginId)
            override fun cancel(loginId: String): CompletableFuture<Boolean> = client.cancelLogin(loginId)
        },
        onStatus,
    )

    @Synchronized
    internal fun login(
        transport: LoginTransport,
        onStatus: (String) -> Unit,
    ): CompletableFuture<Unit> {
        val key = transport.key
        attempts[key]?.takeUnless { it.isDone }?.let { return it }

        val shared = CompletableFuture<Unit>()
        attempts[key] = shared

        transport.start()
            .thenCompose { start ->
                onStatus("Opened browser for AtomGit sign-in.")
                val deadline = System.nanoTime() +
                    TimeUnit.SECONDS.toNanos(start.expiresInSeconds.coerceAtLeast(1).toLong())
                poll(transport, start.loginId, deadline, onStatus)
            }
            .whenComplete { _, error ->
                if (error == null) shared.complete(Unit) else shared.completeExceptionally(unwrap(error))
                attempts.remove(key, shared)
            }
        return shared
    }

    private fun poll(
        transport: LoginTransport,
        loginId: String,
        deadlineNanos: Long,
        onStatus: (String) -> Unit,
    ): CompletableFuture<Unit> {
        if (System.nanoTime() >= deadlineNanos) {
            return transport.cancel(loginId).handle { _, _ -> Unit }.thenCompose {
                CompletableFuture.failedFuture<Unit>(IllegalStateException("Login timed out; start a new login."))
            }
        }

        return transport.poll(loginId)
            .handle { result, error -> PollAttempt(result, error?.let(::unwrap)) }
            .thenCompose { attempt ->
                val error = attempt.error
                if (error != null) {
                    if (error is DaemonHttpException && error.retryable && System.nanoTime() < deadlineNanos) {
                        onStatus("Login service is temporarily unavailable; retrying...")
                        return@thenCompose delayedPoll(transport, loginId, deadlineNanos, onStatus, DEFAULT_LOGIN_POLL_DELAY_MS)
                    }
                    return@thenCompose CompletableFuture.failedFuture<Unit>(error)
                }

                val result = requireNotNull(attempt.result)
                when (result.status) {
                    "authorized" -> {
                        onStatus("Signed in${result.userName?.let { " as $it" } ?: ""}.")
                        CompletableFuture.completedFuture(Unit)
                    }
                    "pending" -> {
                        onStatus("Waiting for browser authorization...")
                        delayedPoll(
                            transport,
                            loginId,
                            deadlineNanos,
                            onStatus,
                            result.retryAfterMs?.toLong() ?: DEFAULT_LOGIN_POLL_DELAY_MS,
                        )
                    }
                    "expired" -> CompletableFuture.failedFuture<Unit>(
                        IllegalStateException(result.message ?: "Login expired; start a new login."),
                    )
                    "cancelled" -> CompletableFuture.failedFuture<Unit>(
                        IllegalStateException(result.message ?: "Login was cancelled."),
                    )
                    "failed" -> CompletableFuture.failedFuture<Unit>(
                        IllegalStateException(result.message ?: "Login failed (${result.code ?: "unknown"})."),
                    )
                    else -> CompletableFuture.failedFuture<Unit>(
                        IllegalStateException("Unexpected login status: ${result.status}"),
                    )
                }
            }
    }

    private fun delayedPoll(
        transport: LoginTransport,
        loginId: String,
        deadlineNanos: Long,
        onStatus: (String) -> Unit,
        delayMs: Long,
    ): CompletableFuture<Unit> =
        CompletableFuture.supplyAsync(
            { Unit },
            CompletableFuture.delayedExecutor(delayMs.coerceAtLeast(100), TimeUnit.MILLISECONDS),
        ).thenCompose { poll(transport, loginId, deadlineNanos, onStatus) }

    companion object {
        fun getInstance(): AtomCodeLoginCoordinator =
            ApplicationManager.getApplication().getService(AtomCodeLoginCoordinator::class.java)
    }
}

internal interface LoginTransport {
    val key: String
    fun start(): CompletableFuture<LoginStartResponse>
    fun poll(loginId: String): CompletableFuture<LoginPollResponse>
    fun cancel(loginId: String): CompletableFuture<Boolean>
}

private data class PollAttempt(
    val result: LoginPollResponse?,
    val error: Throwable?,
)

private fun unwrap(error: Throwable): Throwable {
    var current = error
    while ((current is CompletionException || current is java.util.concurrent.ExecutionException) && current.cause != null) {
        current = requireNotNull(current.cause)
    }
    return current
}
