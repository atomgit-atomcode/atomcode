package com.atomcode.jetbrains.i18n

import java.util.Locale

/**
 * Turns a runtime refusal into something a person can act on.
 *
 * A session has exactly one owning runtime at a time, so opening one that the
 * terminal (or the other IDE) currently holds fails with
 * "session ... is already in use by another runtime". That sentence says nothing
 * about what to do, so it is replaced with a localized explanation.
 */
internal object RuntimeErrorMessages {
    private val SESSION_IN_USE =
        Regex("""session\s+"?([0-9A-Za-z_-]+)"?\s+is already in use by another runtime""")

    fun friendly(message: String, locale: Locale = Locale.getDefault()): String {
        val id = SESSION_IN_USE.find(message)?.groupValues?.get(1) ?: return message
        return AtomCodeBundle.message(locale, "error.sessionInUse", id.take(8))
    }
}