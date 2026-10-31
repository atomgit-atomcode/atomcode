package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.ApprovalMode
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The mode a chat request carries, and where it comes from.
 *
 * `[permissions] default_mode` in config.toml is applied by the daemon; the
 * previous client sent its own `build` default on every request, which the daemon
 * prefers over its own mode (`effective_chat_approval_mode`), so the setting never
 * took effect in the IDE. These pin the rule that replaced it.
 */
class ApprovalModeRuntimeStateTest {
    @Test
    fun `a fresh session sends no mode so the daemon's default applies`() {
        val state = ApprovalModeRuntimeState()

        assertFalse(state.userChoseMode)
        assertNull(state.modeForRequest())
        // The label still has something to show before the daemon answers.
        assertEquals(ApprovalMode.Build, state.displayMode)
    }

    @Test
    fun `a mode a person picks is carried by later requests`() {
        val state = ApprovalModeRuntimeState()

        assertTrue(state.beginSwitch(ApprovalMode.Auto))
        assertTrue(state.userChoseMode)
        // In flight counts: the request states what the person asked for.
        assertEquals(ApprovalMode.Auto, state.modeForRequest())

        state.completeSwitch(ApprovalMode.Auto, ApprovalMode.Auto.wire)
        assertEquals(ApprovalMode.Auto, state.modeForRequest())
    }

    @Test
    fun `reading the daemon's mode is not a choice and stops the override`() {
        val state = ApprovalModeRuntimeState()

        assertEquals(ApprovalMode.Auto, state.refreshFromDaemon(ApprovalMode.Auto.wire))
        assertEquals(ApprovalMode.Auto, state.displayMode)
        // The label follows the daemon, but requests keep deferring to it.
        assertFalse(state.userChoseMode)
        assertNull(state.modeForRequest())
    }

    @Test
    fun `a daemon refresh leaves a switch still in flight alone`() {
        val state = ApprovalModeRuntimeState()

        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        assertEquals(ApprovalMode.Plan, state.refreshFromDaemon(ApprovalMode.Auto.wire))
        assertEquals(ApprovalMode.Plan, state.modeForRequest())
    }

    @Test
    fun `a failed switch returns to the mode that was in force`() {
        val state = ApprovalModeRuntimeState()
        state.refreshFromDaemon(ApprovalMode.AcceptEdits.wire)

        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        assertEquals(ApprovalMode.AcceptEdits, state.failSwitch(ApprovalMode.Plan))
        assertEquals(ApprovalMode.AcceptEdits, state.modeForRequest())
    }

    @Test
    fun `a new conversation drops the previous pick and follows the daemon again`() {
        val state = ApprovalModeRuntimeState()
        state.refreshFromDaemon(ApprovalMode.Auto.wire)

        // The person switches to Build in one conversation …
        assertTrue(state.beginSwitch(ApprovalMode.Build))
        state.completeSwitch(ApprovalMode.Build, ApprovalMode.Build.wire)
        assertEquals(ApprovalMode.Build, state.modeForRequest())

        // … then starts / opens a conversation: config.toml's default_mode rules again.
        state.resetChoice()
        assertFalse(state.userChoseMode)
        assertNull(state.modeForRequest())

        assertEquals(ApprovalMode.Auto, state.refreshFromDaemon(ApprovalMode.Auto.wire))
        assertEquals(ApprovalMode.Auto, state.displayMode)
    }

    @Test
    fun `a new conversation leaves a switch still in flight alone`() {
        val state = ApprovalModeRuntimeState()

        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        state.resetChoice()

        // The in-flight answer is still what the person asked for.
        assertTrue(state.userChoseMode)
        assertEquals(ApprovalMode.Plan, state.modeForRequest())
    }

    @Test
    fun `adopting the daemon default clears the previous pick`() {
        val state = ApprovalModeRuntimeState()
        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        state.completeSwitch(ApprovalMode.Plan, ApprovalMode.Plan.wire)

        // A conversation that starts fresh adopts what the daemon holds.
        assertEquals(ApprovalMode.Auto, state.adoptDaemonDefault(ApprovalMode.Auto.wire))
        assertEquals(ApprovalMode.Auto, state.displayMode)
        assertFalse(state.userChoseMode)
        assertNull(state.modeForRequest())
    }

    @Test
    fun `an unset default_mode falls back to build on both sides`() {
        // config.toml without `[permissions] default_mode`: the daemon seeds its
        // live mode with `ApprovalMode::default()` (build) and reports "build", so
        // the adopted label is build — not a stale mode from the last conversation.
        val state = ApprovalModeRuntimeState()
        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        state.completeSwitch(ApprovalMode.Plan, ApprovalMode.Plan.wire)

        assertEquals(ApprovalMode.Build, state.adoptDaemonDefault(ApprovalMode.Build.wire))
        assertEquals(ApprovalMode.Build, state.displayMode)
        assertFalse(state.userChoseMode)
        assertNull(state.modeForRequest())
    }

    @Test
    fun `adopting the daemon default leaves an in-flight switch alone`() {
        val state = ApprovalModeRuntimeState()

        assertTrue(state.beginSwitch(ApprovalMode.Plan))
        assertEquals(ApprovalMode.Plan, state.adoptDaemonDefault(ApprovalMode.Auto.wire))

        assertTrue(state.userChoseMode)
        assertEquals(ApprovalMode.Plan, state.modeForRequest())
    }

    @Test
    fun `auto travels as the daemon's bypass wire name`() {
        // config.toml says "auto"; the wire says "bypass". A mismatch here would
        // fall back silently to the previous mode (parseApprovalMode's else arm).
        assertEquals("bypass", ApprovalMode.Auto.wire)
    }
}