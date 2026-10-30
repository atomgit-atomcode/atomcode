package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * The effort menu must offer what the selected model declares — the daemon sends
 * `effort_levels` per model, and the wire clamps anything else, which used to
 * read as "I picked Max and nothing changed".
 */
class EffortChoicesTest {
    private fun model(
        provider: String = "AtomGit",
        name: String = "lumen-alpha",
        applicable: Boolean = true,
        levels: List<String> = listOf("low", "medium", "high"),
        current: String? = null,
    ) = ModelInfo(
        provider = provider,
        model = name,
        providerType = "openai_compat",
        isDefault = false,
        effortApplicable = applicable,
        effortLevels = levels,
        reasoningEffort = current,
    )

    @Test
    fun `a model that declares low medium high never offers max`() {
        val labels = EffortChoices.forModel(model()).map { it.label }

        assertEquals(listOf("Default", "Low", "Medium", "High"), labels)
    }

    @Test
    fun `a different model offers a different set`() {
        val deepseek = EffortChoices.forModel(
            model(name = "deepseek-flash", levels = listOf("high", "max")),
        )

        assertEquals(listOf(null, "high", "max"), deepseek.map { it.value })
        assertEquals(listOf("Default", "High", "Max"), deepseek.map { it.label })
    }

    @Test
    fun `a model without effort capability gets no menu`() {
        assertTrue(EffortChoices.forModel(model(applicable = false)).isEmpty())
        assertTrue(EffortChoices.forModel(null).isEmpty())
    }

    @Test
    fun `a model that declares no levels gets no menu at all`() {
        // The daemon derives both fields from the endpoint's declared levels
        // (`declared_effort_levels`: unset ⇒ empty), so "applicable but no levels"
        // describes an endpoint with nothing to switch — offering a lone "Default"
        // row was the reported bug.
        assertTrue(EffortChoices.forModel(model(levels = emptyList())).isEmpty())
        assertTrue(
            EffortChoices.forModel(model(levels = listOf("  "))).isEmpty(),
            "blank levels are not choices",
        )
        assertTrue(
            EffortChoices.forModel(model(levels = listOf("auto"))).isEmpty(),
            "auto alone leaves nothing switchable",
        )
    }

    @Test
    fun `the auto sentinel never becomes a switchable level`() {
        val choices = EffortChoices.forModel(model(levels = listOf("auto", "high")))

        assertEquals(listOf(null, "high"), choices.map { it.value })
    }

    @Test
    fun `the label follows the effort in force and falls back to Default`() {
        val choices = EffortChoices.forModel(model(current = "medium"))

        assertEquals("Medium", EffortChoices.labelFor(choices, "medium"))
        assertEquals("Default", EffortChoices.labelFor(choices, null))
        // A stale level the model no longer declares reads as Default, not a raw token.
        assertEquals("Default", EffortChoices.labelFor(choices, "max"))
    }
}