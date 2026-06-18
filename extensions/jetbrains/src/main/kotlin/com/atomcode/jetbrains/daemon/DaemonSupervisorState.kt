package com.atomcode.jetbrains.daemon

/**
 * Project-level daemon lifecycle state for the reworked architecture.
 *
 * This intentionally lives next to the existing daemon types so the new
 * supervisor can be introduced without first rewriting the current service.
 */
sealed interface DaemonSupervisorState {
    data object Idle : DaemonSupervisorState
    data object Probing : DaemonSupervisorState
    data class Starting(val attempt: Int) : DaemonSupervisorState
    data class Ready(val version: String, val endpoint: String) : DaemonSupervisorState
    data class Degraded(val reason: String) : DaemonSupervisorState
    data class Restarting(val attempt: Int, val reason: String) : DaemonSupervisorState
    data class PortConflict(val host: String, val port: Int, val service: String?) : DaemonSupervisorState
    data class Failed(val kind: ConnectionErrorKind, val message: String) : DaemonSupervisorState
}

enum class DaemonOwnership {
    Unknown,
    External,
    PluginOwned,
}

data class DaemonRuntimeInfo(
    val host: String,
    val port: Int,
    val version: String?,
    val ownership: DaemonOwnership,
    val projectPath: String?,
)
