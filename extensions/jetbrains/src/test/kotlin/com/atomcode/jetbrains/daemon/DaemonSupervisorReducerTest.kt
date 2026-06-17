package com.atomcode.jetbrains.daemon

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull

class DaemonSupervisorReducerTest {
    @Test
    fun `probe succeeded with atomcode daemon enters ready state`() {
        val model = reduceDaemonSupervisor(
            DaemonSupervisorModel(state = DaemonSupervisorState.Probing),
            DaemonSupervisorAction.ProbeSucceeded(
                service = "atomcode-daemon",
                version = "0.1.0",
                endpoint = "http://127.0.0.1:13456",
                ownership = DaemonOwnership.PluginOwned,
            ),
        )

        val state = assertIs<DaemonSupervisorState.Ready>(model.state)
        assertEquals("0.1.0", state.version)
        assertEquals("http://127.0.0.1:13456", state.endpoint)
        assertEquals(0, model.restartAttempt)
        assertNull(model.nextRetryDelayMs)
        assertEquals(DaemonOwnership.PluginOwned, model.runtime?.ownership)
    }

    @Test
    fun `probe succeeded with unexpected service enters port conflict`() {
        val model = reduceDaemonSupervisor(
            DaemonSupervisorModel(state = DaemonSupervisorState.Probing),
            DaemonSupervisorAction.ProbeSucceeded(
                service = "other-service",
                version = "1.0",
                endpoint = "http://localhost:9999",
                ownership = DaemonOwnership.External,
            ),
        )

        val state = assertIs<DaemonSupervisorState.PortConflict>(model.state)
        assertEquals("localhost", state.host)
        assertEquals(9999, state.port)
        assertEquals("other-service", state.service)
        assertNull(model.runtime)
    }

    @Test
    fun `process exit with auto start schedules restart using backoff`() {
        val model = reduceDaemonSupervisor(
            DaemonSupervisorModel(
                state = DaemonSupervisorState.Ready("0.1.0", "http://127.0.0.1:13456"),
                restartAttempt = 0,
            ),
            DaemonSupervisorAction.ProcessExited(reason = "idle timeout", autoStart = true),
            RetryPolicy(delaysMs = listOf(100, 200, 300)),
        )

        val state = assertIs<DaemonSupervisorState.Restarting>(model.state)
        assertEquals(1, state.attempt)
        assertEquals("idle timeout", state.reason)
        assertEquals(100, model.nextRetryDelayMs)
    }

    @Test
    fun `process exit without auto start becomes degraded`() {
        val model = reduceDaemonSupervisor(
            DaemonSupervisorModel(state = DaemonSupervisorState.Ready("0.1.0", "http://127.0.0.1:13456")),
            DaemonSupervisorAction.ProcessExited(reason = "idle timeout", autoStart = false),
        )

        val state = assertIs<DaemonSupervisorState.Degraded>(model.state)
        assertEquals("idle timeout", state.reason)
        assertNull(model.nextRetryDelayMs)
    }

    @Test
    fun `version mismatch requests immediate restart`() {
        val model = reduceDaemonSupervisor(
            DaemonSupervisorModel(restartAttempt = 2),
            DaemonSupervisorAction.VersionMismatch(runningVersion = "0.1.0", expectedVersion = "0.2.0"),
        )

        val state = assertIs<DaemonSupervisorState.Restarting>(model.state)
        assertEquals(3, state.attempt)
        assertEquals(0, model.nextRetryDelayMs)
    }

    @Test
    fun `store notifies subscribers with latest model`() {
        val store = DaemonSupervisorStore()
        val seen = mutableListOf<DaemonSupervisorState>()
        val subscription = store.subscribe { seen += it.state }

        store.dispatch(DaemonSupervisorAction.ProbeStarted)
        subscription.close()
        store.dispatch(DaemonSupervisorAction.ProbeFailed("offline"))

        assertEquals(listOf(DaemonSupervisorState.Idle, DaemonSupervisorState.Probing), seen)
    }
}
