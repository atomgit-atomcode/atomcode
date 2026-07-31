package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.ApprovalMode
import com.atomcode.jetbrains.daemon.AuthStatusResponse
import com.atomcode.jetbrains.daemon.ProviderInfo
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class AtomCodeProjectServiceTest {
    private fun provider(name: String, requiresLogin: Boolean?): ProviderInfo =
        ProviderInfo(
            name = name,
            type = "openai",
            model = "model",
            isDefault = true,
            hasApiKey = false,
            requiresLogin = requiresLogin,
            thinkingEnabled = false,
            thinkingBudget = null,
            thinkingType = null,
            thinkingKeep = null,
        )

    private val signedOut = AuthStatusResponse(
        loggedIn = false,
        expired = false,
        authPath = "/tmp/auth.toml",
        userName = null,
    )

    @Test
    fun `setup is required only when the selected provider depends on login`() {
        assertEquals(false, providerSetupRequired(listOf(provider("custom", false)), "custom", signedOut))
        assertEquals(true, providerSetupRequired(listOf(provider("gateway", true)), "gateway", signedOut))
        assertEquals(true, providerSetupRequired(listOf(provider("legacy", null)), "legacy", signedOut))
        assertEquals(true, providerSetupRequired(emptyList(), "", signedOut))
    }

    @Test
    fun `approval mode runtime state keeps confirmed mode while switch is pending`() {
        val state = ApprovalModeRuntimeState()

        assertEquals(true, state.beginSwitch(ApprovalMode.Plan))
        assertEquals(ApprovalMode.Build, state.confirmedMode)
        assertEquals(ApprovalMode.Plan, state.displayMode)
        assertEquals(ApprovalMode.Plan, state.pendingMode)

        assertEquals(false, state.beginSwitch(ApprovalMode.Auto))
        assertEquals(ApprovalMode.Build, state.confirmedMode)
        assertEquals(ApprovalMode.Plan, state.displayMode)
        assertEquals(ApprovalMode.Plan, state.pendingMode)
    }

    @Test
    fun `approval mode runtime state ignores refresh while switch is pending`() {
        val state = ApprovalModeRuntimeState()

        state.beginSwitch(ApprovalMode.Plan)
        state.refreshFromDaemon(ApprovalMode.Auto.wire)

        assertEquals(ApprovalMode.Build, state.confirmedMode)
        assertEquals(ApprovalMode.Plan, state.displayMode)
        assertEquals(ApprovalMode.Plan, state.pendingMode)
    }

    @Test
    fun `approval mode runtime state completes and rolls back pending switch`() {
        val state = ApprovalModeRuntimeState()

        state.beginSwitch(ApprovalMode.Plan)
        assertEquals(ApprovalMode.Plan, state.completeSwitch(ApprovalMode.Plan, ApprovalMode.Plan.wire))
        assertEquals(ApprovalMode.Plan, state.confirmedMode)
        assertEquals(ApprovalMode.Plan, state.displayMode)
        assertNull(state.pendingMode)

        state.beginSwitch(ApprovalMode.Auto)
        assertEquals(ApprovalMode.Plan, state.failSwitch(ApprovalMode.Auto))
        assertEquals(ApprovalMode.Plan, state.confirmedMode)
        assertEquals(ApprovalMode.Plan, state.displayMode)
        assertNull(state.pendingMode)
    }

    @Test
    fun `approval mode runtime state parses accept edits and auto wire values`() {
        val state = ApprovalModeRuntimeState()

        assertEquals(ApprovalMode.AcceptEdits, state.refreshFromDaemon("accept_edits"))
        assertEquals(ApprovalMode.AcceptEdits, state.confirmedMode)

        assertEquals(ApprovalMode.Auto, state.refreshFromDaemon("bypass"))
        assertEquals(ApprovalMode.Auto, state.confirmedMode)
    }

    @Test
    fun `approval mode runtime state rolls back when daemon response mode is unknown`() {
        val state = ApprovalModeRuntimeState()

        state.beginSwitch(ApprovalMode.AcceptEdits)
        assertEquals(
            ApprovalMode.Build,
            state.completeSwitch(ApprovalMode.AcceptEdits, "unsupported_mode"),
        )
        assertEquals(ApprovalMode.Build, state.confirmedMode)
        assertEquals(ApprovalMode.Build, state.displayMode)
        assertNull(state.pendingMode)
    }

    @Test
    fun `background health probes when no endpoint has connected yet`() {
        val current = DaemonConnectionKey(DaemonEndpointKey("127.0.0.1", 13456), "", true, 30_000)

        assertEquals(false, shouldResetBackgroundConnection(null, current))
    }

    @Test
    fun `background health resets an established connection after settings change`() {
        val connected = DaemonConnectionKey(DaemonEndpointKey("127.0.0.1", 13456), "/first", true, 30_000)
        val current = DaemonConnectionKey(DaemonEndpointKey("127.0.0.1", 13456), "/second", true, 30_000)

        assertEquals(true, shouldResetBackgroundConnection(connected, current))
    }

}
