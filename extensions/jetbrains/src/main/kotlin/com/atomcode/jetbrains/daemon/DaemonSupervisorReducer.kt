package com.atomcode.jetbrains.daemon

data class DaemonSupervisorModel(
    val state: DaemonSupervisorState = DaemonSupervisorState.Idle,
    val runtime: DaemonRuntimeInfo? = null,
    val restartAttempt: Int = 0,
    val nextRetryDelayMs: Long? = null,
)

sealed interface DaemonSupervisorAction {
    data object ProbeStarted : DaemonSupervisorAction
    data class ProbeSucceeded(
        val service: String,
        val version: String,
        val endpoint: String,
        val ownership: DaemonOwnership,
    ) : DaemonSupervisorAction
    data class ProbeFailed(val message: String) : DaemonSupervisorAction
    data class StartRequested(val attempt: Int) : DaemonSupervisorAction
    data class StartFailed(val kind: ConnectionErrorKind, val message: String) : DaemonSupervisorAction
    data class ProcessExited(val reason: String, val autoStart: Boolean) : DaemonSupervisorAction
    data class VersionMismatch(val runningVersion: String, val expectedVersion: String) : DaemonSupervisorAction
    data class PortConflict(val host: String, val port: Int, val service: String?) : DaemonSupervisorAction
    data object StopRequested : DaemonSupervisorAction
}

data class RetryPolicy(
    val delaysMs: List<Long> = listOf(1_000, 2_000, 5_000, 10_000, 30_000),
) {
    fun delayForAttempt(attempt: Int): Long =
        delaysMs[(attempt - 1).coerceAtLeast(0).coerceAtMost(delaysMs.lastIndex)]
}

internal fun reduceDaemonSupervisor(
    model: DaemonSupervisorModel,
    action: DaemonSupervisorAction,
    retryPolicy: RetryPolicy = RetryPolicy(),
): DaemonSupervisorModel =
    when (action) {
        DaemonSupervisorAction.ProbeStarted -> model.copy(
            state = DaemonSupervisorState.Probing,
            nextRetryDelayMs = null,
        )
        is DaemonSupervisorAction.ProbeSucceeded -> {
            if (action.service != "atomcode-daemon") {
                model.copy(
                    state = DaemonSupervisorState.PortConflict(
                        host = endpointHost(action.endpoint),
                        port = endpointPort(action.endpoint),
                        service = action.service,
                    ),
                    runtime = null,
                    nextRetryDelayMs = null,
                )
            } else {
                model.copy(
                    state = DaemonSupervisorState.Ready(action.version, action.endpoint),
                    runtime = DaemonRuntimeInfo(
                        host = endpointHost(action.endpoint),
                        port = endpointPort(action.endpoint),
                        version = action.version,
                        ownership = action.ownership,
                        projectPath = null,
                    ),
                    restartAttempt = 0,
                    nextRetryDelayMs = null,
                )
            }
        }
        is DaemonSupervisorAction.ProbeFailed -> model.copy(
            state = DaemonSupervisorState.Degraded(action.message),
            runtime = null,
            nextRetryDelayMs = null,
        )
        is DaemonSupervisorAction.StartRequested -> model.copy(
            state = DaemonSupervisorState.Starting(action.attempt),
            restartAttempt = action.attempt,
            nextRetryDelayMs = null,
        )
        is DaemonSupervisorAction.StartFailed -> model.copy(
            state = DaemonSupervisorState.Failed(action.kind, action.message),
            runtime = null,
            nextRetryDelayMs = retryPolicy.delayForAttempt(model.restartAttempt.coerceAtLeast(1)),
        )
        is DaemonSupervisorAction.ProcessExited -> {
            if (action.autoStart) {
                val attempt = model.restartAttempt + 1
                model.copy(
                    state = DaemonSupervisorState.Restarting(attempt, action.reason),
                    runtime = null,
                    restartAttempt = attempt,
                    nextRetryDelayMs = retryPolicy.delayForAttempt(attempt),
                )
            } else {
                model.copy(
                    state = DaemonSupervisorState.Degraded(action.reason),
                    runtime = null,
                    nextRetryDelayMs = null,
                )
            }
        }
        is DaemonSupervisorAction.VersionMismatch -> model.copy(
            state = DaemonSupervisorState.Restarting(
                attempt = model.restartAttempt + 1,
                reason = "Daemon version mismatch: running ${action.runningVersion}, expected ${action.expectedVersion}",
            ),
            runtime = null,
            restartAttempt = model.restartAttempt + 1,
            nextRetryDelayMs = 0,
        )
        is DaemonSupervisorAction.PortConflict -> model.copy(
            state = DaemonSupervisorState.PortConflict(action.host, action.port, action.service),
            runtime = null,
            nextRetryDelayMs = null,
        )
        DaemonSupervisorAction.StopRequested -> DaemonSupervisorModel()
    }

private fun endpointHost(endpoint: String): String =
    endpoint.removePrefix("http://")
        .removePrefix("https://")
        .substringBefore(":")
        .ifBlank { "127.0.0.1" }

private fun endpointPort(endpoint: String): Int =
    endpoint.removePrefix("http://")
        .removePrefix("https://")
        .substringAfter(":", missingDelimiterValue = "13456")
        .substringBefore("/")
        .toIntOrNull() ?: 13456
