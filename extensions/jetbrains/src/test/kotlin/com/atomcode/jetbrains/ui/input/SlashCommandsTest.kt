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

        assertEquals(items.size, SlashCommands.match(items, "/").size)
        assertEquals(items.size, SlashCommands.match(items, "").size)
        assertEquals(listOf("/login", "/logout"), SlashCommands.match(items, "/log").map { it.name })
        assertEquals(emptyList(), SlashCommands.match(items, "/nope"))
    }

    @Test
    fun `matching is case insensitive`() {
        assertEquals(
            listOf("/whoami"),
            SlashCommands.match(SlashCommands.BUILT_INS, "/Who").map { it.name },
        )
    }

    @Test
    fun `a prefix hit outranks a subsequence hit`() {
        val skills = listOf(
            SkillInfo("karpathy-guidelines", "guidelines"),
            SkillInfo("kag-tool", "starts with the query"),
        )
        val items = SlashCommands.items(skills)

        // `/ka` is a prefix of `kag-tool` (a skill without the namespace prefix) and
        // only a subsequence of `karpathy-guidelines`; the prefix one must come first.
        val matched = SlashCommands.match(items.filter { it.name.startsWith("/k") }, "/kag")
        // 两个都会命中（karpathy 里 k→a→g 是子序列），但前缀命中必须排在前面。
        assertEquals("/kag-tool", matched.first().name)
        assertTrue(matched.size >= 2, "子序列命中仍然保留，只是排在后面")
    }

    @Test
    fun `a subsequence query still finds a skill`() {
        val items = SlashCommands.items(
            listOf(SkillInfo("andrej-karpathy-skills:karpathy-guidelines", "guidelines")),
        )

        // VS Code 的规则是前缀匹配；这里额外允许子序列，`/kg` 才能命中 karpathy-guidelines。
        assertEquals(
            listOf("/andrej-karpathy-skills:karpathy-guidelines"),
            SlashCommands.match(items, "/kg").map { it.name },
        )
    }

    @Test
    fun `an unmatched subsequence disappears`() {
        val items = SlashCommands.items(listOf(SkillInfo("silent-skill", "x")))

        assertEquals(emptyList(), SlashCommands.match(items, "/zzz"))
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
