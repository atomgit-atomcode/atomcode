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
    fun `auto travels as the daemon's bypass wire name`() {
        // config.toml says "auto"; the wire says "bypass". A mismatch here would
        // fall back silently to the previous mode (parseApprovalMode's else arm).
        assertEquals("bypass", ApprovalMode.Auto.wire)
    }
}