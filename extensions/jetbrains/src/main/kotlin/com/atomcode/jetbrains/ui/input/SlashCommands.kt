package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.SkillInfo

/** One row of the slash-command menu. */
internal data class SlashCommand(val name: String, val description: String)

/**
 * What the `/` menu offers: the commands this client can run itself, followed by
 * every user-invocable skill the daemon exposes.
 *
 * Mirrors the VS Code picker: built-ins first (they are handled locally), then the
 * skills from `GET /skills`. A skill whose name collides with a built-in is dropped
 * — the local handler wins, so listing it would offer a command that never runs.
 */
internal object SlashCommands {
    /** Fallback description for a skill that declares none. */
    const val SKILL_FALLBACK_DESCRIPTION = "运行该技能"

    val BUILT_INS: List<SlashCommand> = listOf(
        SlashCommand("/login", "登录 AtomGit"),
        SlashCommand("/logout", "退出 AtomGit 登录"),
        SlashCommand("/whoami", "显示当前登录用户"),
        SlashCommand("/status", "显示会话状态"),
        SlashCommand("/config", "显示配置文件路径"),
        SlashCommand("/reload", "重新读取配置文件"),
        SlashCommand("/review", "审查代码"),
    )

    fun items(skills: List<SkillInfo>): List<SlashCommand> {
        val localNames = BUILT_INS.map { it.name.removePrefix("/") }.toSet()
        val skillItems = skills
            .filter { it.name.isNotBlank() && it.name !in localNames }
            .map { SlashCommand("/${it.name}", it.description.ifBlank { SKILL_FALLBACK_DESCRIPTION }) }
        return BUILT_INS + skillItems
    }

    /**
     * Commands matching what the person typed after `/`.
     *
     * Same rule as the VS Code picker — a name that starts with the query ranks
     * first — plus a subsequence fallback so `/kg` still finds
     * `karpathy-guidelines`. A bare `/` lists everything, in declaration order.
     */
    fun match(items: List<SlashCommand>, prefix: String): List<SlashCommand> {
        val needle = prefix.trim().removePrefix("/").lowercase()
        if (needle.isEmpty()) return items
        return items
            .mapIndexedNotNull { index, item ->
                score(item.name, needle)?.let { Triple(item, it, index) }
            }
            .sortedWith(compareByDescending<Triple<SlashCommand, Int, Int>> { it.second }.thenBy { it.third })
            .map { it.first }
    }

    /**
     * Higher is better, `null` means "no match". A prefix hit beats any
     * subsequence hit (that is the VS Code ordering); within the same kind,
     * tighter matches (shorter name, fewer gaps) win.
     */
    fun score(name: String, needle: String): Int? {
        val haystack = name.removePrefix("/").lowercase()
        if (haystack.startsWith(needle)) return PREFIX_SCORE - haystack.length
        var cursor = 0
        var gaps = 0
        for (char in needle) {
            val hit = haystack.indexOf(char, cursor)
            if (hit < 0) return null
            gaps += hit - cursor
            cursor = hit + 1
        }
        return SUBSEQUENCE_SCORE - gaps
    }

    private const val PREFIX_SCORE = 1_000
    private const val SUBSEQUENCE_SCORE = 100

    /** The command word of an input line, lowercased, or null when it is not one. */
    fun commandWord(prompt: String): String? {
        val word = prompt.trim().split(Regex("\\s+"), limit = 2).firstOrNull() ?: return null
        return word.lowercase().takeIf { it.startsWith("/") }
    }
}
