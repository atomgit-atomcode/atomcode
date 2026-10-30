package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo

/** One row of the effort menu; `value == null` is the "Default" entry. */
internal data class EffortChoice(val value: String?, val label: String)

/**
 * What the effort menu offers, derived from the selected model.
 *
 * The daemon sends `effort_levels` per model, so a model declaring
 * `low/medium/high` must never offer `max` — the wire clamps such a value anyway,
 * which reads as "I picked Max and nothing changed". A model that declares no
 * levels keeps every level the wire accepts, and a model without effort
 * capability gets no menu at all.
 */
internal object EffortChoices {
    /** Every level the wire accepts, used when an endpoint declares none. */
    val FALLBACK_LEVELS = listOf("low", "medium", "high", "xhigh", "max")

    const val DEFAULT_LABEL = "Default"

    fun forModel(model: ModelInfo?): List<EffortChoice> {
        if (model == null || !model.effortApplicable) return emptyList()
        // `auto` means "send nothing", so it is never a switchable level.
        val declared = model.effortLevels.filter { it.isNotBlank() && it != "auto" }
        val levels = declared.ifEmpty { FALLBACK_LEVELS }
        return listOf(EffortChoice(null, DEFAULT_LABEL)) +
            levels.map { EffortChoice(it, labelOf(it)) }
    }

    /** Display form of a level; the wire keeps it lowercase. */
    fun labelOf(level: String): String = level.replaceFirstChar { it.uppercaseChar() }

    /** Label for the effort in force, falling back to "Default" when unset. */
    fun labelFor(choices: List<EffortChoice>, current: String?): String =
        choices.firstOrNull { it.value == current }?.label ?: DEFAULT_LABEL
}