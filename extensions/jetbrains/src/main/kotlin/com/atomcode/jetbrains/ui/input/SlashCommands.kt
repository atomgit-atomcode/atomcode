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
     * Commands whose name starts with `prefix`. A bare `/` (or an empty filter)
     * lists everything, the way the popup opens before a second character arrives.
     */
    fun filter(items: List<SlashCommand>, prefix: String): List<SlashCommand> {
        val needle = prefix.trim().lowercase()
        if (needle.isEmpty() || needle == "/") return items
        return items.filter { it.name.lowercase().startsWith(needle) }
    }

    /** The command word of an input line, lowercased, or null when it is not one. */
    fun commandWord(prompt: String): String? {
        val word = prompt.trim().split(Regex("\\s+"), limit = 2).firstOrNull() ?: return null
        return word.lowercase().takeIf { it.startsWith("/") }
    }
}
