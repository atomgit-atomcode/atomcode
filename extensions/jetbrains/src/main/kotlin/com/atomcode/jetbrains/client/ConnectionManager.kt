package com.atomcode.jetbrains.client

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

sealed interface ConnectionState {
    data object Idle : ConnectionState
    data object Checking : ConnectionState
    data object Starting : ConnectionState
    data object Restarting : ConnectionState
    data class Ready(val version: String) : ConnectionState
    data class Error(val message: String) : ConnectionState
}

class ConnectionManager(
    private val client: DaemonClient,
    private val expectedVersion: String,
    private val scope: CoroutineScope,
    private val healthIntervalMs: Long = 30_000L,
    private val onStartDaemon: suspend () -> Unit = {},
) {
    private val _state = MutableStateFlow<ConnectionState>(ConnectionState.Idle)
    val state: StateFlow<ConnectionState> = _state.asStateFlow()

    init {
        scope.launch {
            delay(5_000)
            while (isActive) {
                checkHealth()
                delay(healthIntervalMs)
            }
        }
    }

    suspend fun ensureConnected(): ConnectionState {
        _state.value = ConnectionState.Checking
        return try {
            val health = client.health()
            if (health.version != expectedVersion) {
                _state.value = ConnectionState.Restarting
                client.shutdown()
                delay(1_000)
                onStartDaemon()
            }
            _state.value = ConnectionState.Ready(health.version)
            _state.value
        } catch (_: Exception) {
            _state.value = ConnectionState.Starting
            onStartDaemon()
            _state.value = ConnectionState.Ready(expectedVersion)
            _state.value
        }
    }

    private suspend fun checkHealth() {
        try {
            val health = client.health()
            if (_state.value !is ConnectionState.Ready) {
                _state.value = ConnectionState.Ready(health.version)
            }
        } catch (e: Exception) {
            _state.value = ConnectionState.Error("Health check failed: ${e.message}")
        }
    }
}
