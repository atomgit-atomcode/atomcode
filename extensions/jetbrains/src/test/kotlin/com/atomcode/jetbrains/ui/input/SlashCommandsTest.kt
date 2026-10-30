package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.SkillInfo
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The `/` menu must offer what the client can actually run plus the skills the
 * daemon exposes. The JetBrains picker used to hardcode `/login` and `/review`,
 * so `/logout`, `/whoami`, `/status`, `/config`, `/reload` and every installed
 * skill were unreachable even though the VS Code client listed them.
 */
class SlashCommandsTest {
    @Test
    fun `the built-ins match the vscode picker`() {
        // Six shared with VS Code plus the JetBrains-only review prompt.
        assertEquals(
            listOf("/login", "/logout", "/whoami", "/status", "/config", "/reload", "/review"),
            SlashCommands.BUILT_INS.map { it.name },
        )
        assertTrue(
            SlashCommands.BUILT_INS.all { it.description.isNotBlank() },
            "every built-in needs a description, the menu renders it",
        )
    }

    @Test
    fun `installed skills are appended after the built-ins`() {
        val items = SlashCommands.items(
            listOf(
                SkillInfo("andrej-karpathy-skills:karpathy-guidelines", "Behavioral guidelines"),
                SkillInfo("ascend-model-agent-plugin:adapt-agent", "Model adaptation"),
            ),
        )

        assertEquals(SlashCommands.BUILT_INS.size + 2, items.size)
        assertEquals(
            listOf(
                "/andrej-karpathy-skills:karpathy-guidelines",
                "/ascend-model-agent-plugin:adapt-agent",
            ),
            items.drop(SlashCommands.BUILT_INS.size).map { it.name },
        )
        // The daemon's description survives; only the fallback replaces a missing one.
        assertEquals("Behavioral guidelines", items[SlashCommands.BUILT_INS.size].description)
        assertEquals("Model adaptation", items.last().description)
    }

    @Test
    fun `a skill that collides with a built-in is dropped`() {
        val items = SlashCommands.items(listOf(SkillInfo("login", "shadowed skill")))

        assertEquals(SlashCommands.BUILT_INS.size, items.size)
        assertEquals(1, items.count { it.name == "/login" })
        assertEquals("登录 AtomGit", items.first { it.name == "/login" }.description)
    }

    @Test
    fun `a skill without a description still reads as a skill`() {
        val items = SlashCommands.items(listOf(SkillInfo("silent-skill", "  ")))

        assertEquals(
            SlashCommands.SKILL_FALLBACK_DESCRIPTION,
            items.last { it.name == "/silent-skill" }.description,
        )
    }

    @Test
    fun `blank skill names never become commands`() {
        val items = SlashCommands.items(listOf(SkillInfo("", "no name"), SkillInfo("   ", "blank")))

        assertEquals(SlashCommands.BUILT_INS.size, items.size)
    }

    @Test
    fun `a bare slash lists everything and a prefix narrows it`() {
        val items = SlashCommands.BUILT_INS

        assertEquals(items.size, SlashCommands.filter(items, "/").size)
        assertEquals(items.size, SlashCommands.filter(items, "").size)
        assertEquals(listOf("/login", "/logout"), SlashCommands.filter(items, "/log").map { it.name })
        assertEquals(emptyList(), SlashCommands.filter(items, "/nope"))
    }

    @Test
    fun `the filter is case insensitive`() {
        assertEquals(
            listOf("/whoami"),
            SlashCommands.filter(SlashCommands.BUILT_INS, "/Who").map { it.name },
        )
    }

    @Test
    fun `the command word is the lowercased first token`() {
        assertEquals("/login", SlashCommands.commandWord("/login"))
        assertEquals("/status", SlashCommands.commandWord("  /Status  "))
        assertEquals("/config", SlashCommands.commandWord("/config now"))
        assertNull(SlashCommands.commandWord("看起来如何"))
        assertNull(SlashCommands.commandWord(""))
    }
}
