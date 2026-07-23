package com.atomcode.jetbrains.daemon

import kotlin.test.Test
import kotlin.test.assertEquals

class AtomCodeDaemonTypesTest {
    @Test
    fun `approval modes keep canonical order display names and wire values`() {
        assertEquals(
            listOf(
                Triple(ApprovalMode.Build, "Build", "build"),
                Triple(ApprovalMode.AcceptEdits, "Accept Edits", "accept_edits"),
                Triple(ApprovalMode.Auto, "Auto", "bypass"),
                Triple(ApprovalMode.Plan, "Plan", "plan"),
            ),
            ApprovalMode.values().map { Triple(it, it.toString(), it.wire) },
        )
    }

    @Test
    fun `ProviderInfo toString with default provider`() {
        val info = ProviderInfo("p1", "openai", "gpt-4", isDefault = true, hasApiKey = true, thinkingEnabled = false, thinkingBudget = null, thinkingType = null, thinkingKeep = null)
        assertEquals("p1 - gpt-4 *", info.toString())
    }

    @Test
    fun `ProviderInfo toString with non-default provider`() {
        val info = ProviderInfo("p1", "openai", "gpt-4", isDefault = false, hasApiKey = true, thinkingEnabled = false, thinkingBudget = null, thinkingType = null, thinkingKeep = null)
        assertEquals("p1 - gpt-4", info.toString())
    }

    @Test
    fun `ProviderInfo toString with thinking enabled`() {
        val info = ProviderInfo("p1", "openai", "gpt-4", isDefault = false, hasApiKey = true, thinkingEnabled = true, thinkingBudget = null, thinkingType = null, thinkingKeep = null)
        assertEquals("p1 - gpt-4 thinking", info.toString())
    }

    @Test
    fun `ProviderInfo toString with default and thinking`() {
        val info = ProviderInfo("p1", "openai", "gpt-4", isDefault = true, hasApiKey = true, thinkingEnabled = true, thinkingBudget = null, thinkingType = null, thinkingKeep = null)
        assertEquals("p1 - gpt-4 * thinking", info.toString())
    }

    @Test
    fun `ProviderInfo toString with neither default nor thinking`() {
        val info = ProviderInfo("p1", "openai", "gpt-4", isDefault = false, hasApiKey = false, thinkingEnabled = false, thinkingBudget = null, thinkingType = null, thinkingKeep = null)
        assertEquals("p1 - gpt-4", info.toString())
    }

    @Test
    fun `ModelInfo toString with default model`() {
        val info = ModelInfo("provider1", "claude-4", "claude", isDefault = true)
        assertEquals("provider1 - claude-4 *", info.toString())
    }

    @Test
    fun `ModelInfo toString with non-default model`() {
        val info = ModelInfo("provider1", "claude-4", "claude", isDefault = false)
        assertEquals("provider1 - claude-4", info.toString())
    }

    @Test
    fun `SessionMeta displayName returns name when present`() {
        val meta = SessionMeta(id = "abc123", name = "My Session", projectHash = "hash1", updatedAt = 1000L, messageCount = 5)
        assertEquals("My Session", meta.displayName)
    }

    @Test
    fun `SessionMeta displayName falls back to first 8 of id when name blank`() {
        val meta = SessionMeta(id = "abcdefghijklmnop", name = "", projectHash = "hash1", updatedAt = 1000L, messageCount = 5)
        assertEquals("abcdefgh", meta.displayName)
    }

    @Test
    fun `SessionMeta displayName works with short id when name blank`() {
        val meta = SessionMeta(id = "abc", name = "", projectHash = "hash1", updatedAt = 1000L, messageCount = 5)
        assertEquals("abc", meta.displayName)
    }

    @Test
    fun `SessionMeta toString combines displayName and messageCount`() {
        val meta = SessionMeta(id = "id123456", name = "Chat", projectHash = "hash1", updatedAt = 1000L, messageCount = 8)
        assertEquals("Chat (8)", meta.toString())
    }

    @Test
    fun `SessionMeta toString uses id prefix when name blank`() {
        val meta = SessionMeta(id = "abcdefghij", name = "", projectHash = "hash1", updatedAt = 1000L, messageCount = 3)
        assertEquals("abcdefgh (3)", meta.toString())
    }

    @Test
    fun `MessageInfo preserves internal origin`() {
        val info = MessageInfo(
            role = "assistant",
            content = "No verification is needed.",
            internalOrigin = "verify_cadence",
        )

        assertEquals("verify_cadence", info.internalOrigin)
    }
}
