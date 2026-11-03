package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo

/** One row of the effort menu; `value == null` is the "Default" entry. */
internal data class EffortChoice(val value: String?, val label: String)

/**
 * What the effort menu offers, derived from the selected model.
 *
 * The daemon sends `effort_applicable` plus `effort_levels` per model, both
 * derived from the endpoint's configured `reasoning_effort_levels`
 * (`atomcode-config` `declared_effort_levels`: unset ⇒ empty). So "model
 * declares no levels" means the endpoint has no switchable effort: there is
 * nothing to pick, and the selector must disappear instead of offering a lone
 * "Default" row — that read as "the option is there but does nothing".
 *
 * A model declaring `low/medium/high` must never offer `max`: the wire clamps
 * such a value, which reads as "I picked Max and nothing changed".
 */
internal object EffortChoices {
    const val DEFAULT_LABEL = "Default"

    fun forModel(model: ModelInfo?): List<EffortChoice> {
        if (model == null || !model.effortApplicable) return emptyList()
        // `auto` means "send nothing", so it is never a switchable level.
        val levels = model.effortLevels.filter { it.isNotBlank() && it != "auto" }
        // No levels ⇒ no selector. The daemon reports both fields from the same
        // config, so this only triggers for an endpoint that declares nothing.
        if (levels.isEmpty()) return emptyList()
        return listOf(EffortChoice(null, DEFAULT_LABEL)) +
            levels.map { EffortChoice(it, labelOf(it)) }
    }

    /** Display form of a level; the wire keeps it lowercase. */
    fun labelOf(level: String): String = level.replaceFirstChar { it.uppercaseChar() }

    /** Label for the effort in force, falling back to "Default" when unset. */
    fun labelFor(choices: List<EffortChoice>, current: String?): String =
        choices.firstOrNull { it.value == current }?.label ?: DEFAULT_LABEL
}