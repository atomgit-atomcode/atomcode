package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.AtomCodeDaemonClient
import com.atomcode.jetbrains.daemon.DaemonHttpException
import com.atomcode.jetbrains.daemon.LoginPollResponse
import com.atomcode.jetbrains.daemon.LoginStartResponse
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.Service
import com.intellij.openapi.diagnostic.Logger
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CompletionException
import java.util.concurrent.TimeUnit

private const val DEFAULT_LOGIN_POLL_DELAY_MS = 2_000L

/** Application-wide single-flight owner for daemon OAuth attempts. */
@Service(Service.Level.APP)
class AtomCodeLoginCoordinator {
    private val attempts = mutableMapOf<String, LoginAttempt>()
    private val logger = Logger.getInstance(AtomCodeLoginCoordinator::class.java)

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

    internal fun login(
        transport: LoginTransport,
        onStatus: (String) -> Unit,
    ): CompletableFuture<Unit> {
        val key = transport.key
        val (attempt, startsAttempt) = synchronized(attempts) {
            attempts[key]?.takeUnless { it.future.isDone }?.let { return@synchronized it to false }
            LoginAttempt(CompletableFuture()).also { attempts[key] = it } to true
        }
        attempt.addListener(onStatus)
        if (!startsAttempt) return attempt.future

        transport.start()
            .thenCompose { start ->
                attempt.publish("Opened browser for AtomGit sign-in.")
                val deadline = System.nanoTime() +
                    TimeUnit.SECONDS.toNanos(start.expiresInSeconds.coerceAtLeast(1).toLong())
                poll(transport, start.loginId, deadline, attempt::publish)
            }
            .whenComplete { _, error ->
                if (error == null) attempt.future.complete(Unit) else attempt.future.completeExceptionally(unwrap(error))
                synchronized(attempts) {
                    if (attempts[key] === attempt) attempts.remove(key)
                }
            }
        return attempt.future
    }

    private inner class LoginAttempt(
        val future: CompletableFuture<Unit>,
    ) {
        private val listeners = mutableListOf<(String) -> Unit>()
        private var latestStatus: String? = null

        fun addListener(listener: (String) -> Unit) {
            synchronized(this) {
                listeners += listener
                latestStatus?.let { notifyListener(listener, it) }
            }
        }

        fun publish(status: String) {
            synchronized(this) {
                latestStatus = status
                listeners.forEach { notifyListener(it, status) }
            }
        }

        private fun notifyListener(listener: (String) -> Unit, status: String) {
            try {
                listener(status)
            } catch (error: Exception) {
                logger.warn("AtomCode login status listener failed", error)
            }
        }
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
