package com.atomcode.jetbrains.i18n

import java.util.Locale
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * A session belongs to one runtime at a time, so an IDE opening one the terminal
 * holds gets a lease refusal. That message must come out actionable, in the UI's
 * language, and every other error must pass through untouched.
 */
class RuntimeErrorMessagesTest {
    private val inUse =
        "coding runtime prepare failed: session " +
            "\"0f1b59ae-b738-4e95-80fd-6c0525b1a660\" is already in use by another runtime"

    @Test
    fun `the refusal is explained in Chinese with a short session id`() {
        val message = RuntimeErrorMessages.friendly(inUse, Locale.CHINESE)

        assertTrue(message.contains("终端"), message)
        assertTrue(message.contains("0f1b59ae"), message)
        assertTrue(!message.contains("already in use"), message)
    }

    @Test
    fun `the refusal is explained in English too`() {
        val message = RuntimeErrorMessages.friendly(inUse, Locale.ENGLISH)

        assertTrue(message.contains("another client"), message)
        assertTrue(message.contains("0f1b59ae"), message)
    }

    @Test
    fun `the unquoted store wording is recognised as well`() {
        val message = RuntimeErrorMessages.friendly(
            "session s1 is already in use by another runtime",
            Locale.ENGLISH,
        )

        assertTrue(message.contains("another client"), message)
    }

    @Test
    fun `every other error keeps the runtime's own wording`() {
        for (raw in listOf(
            "stream closed before the response completed",
            "session \"abc\" failed to load",
            "coding runtime provider failed: 401 unauthorized",
            "",
        )) {
            assertEquals(raw, RuntimeErrorMessages.friendly(raw, Locale.ENGLISH))
        }
    }
}