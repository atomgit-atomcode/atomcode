package com.atomcode.app

import android.content.Context
import java.io.File

/**
 * Where the session log lives: shared storage or the app's private tree.
 *
 * Shared is the default because it lets a daemon started by hand read the same
 * history. The cost is real — `/storage/emulated/0` is readable by every app
 * holding storage access, and a session log *is* the whole conversation.
 *
 * The switch is a marker file because the choice is made in Rust (the host reads
 * it before pinning the session root) while the toggle lives here.
 */
object SessionsLocation {
    private const val MARKER = "sessions-private"

    /** Whether sessions are currently kept app-private. */
    fun isPrivate(context: Context): Boolean = marker(context).exists()

    /**
     * Move the session root. Takes effect on the next process start: the host
     * pins the root once, before the runtime exists, so a running app keeps the
     * directory it started with.
     */
    fun setPrivate(context: Context, private: Boolean) {
        val file = marker(context)
        if (private) {
            file.parentFile?.mkdirs()
            file.writeText("1")
        } else {
            file.delete()
        }
    }

    private fun marker(context: Context): File =
        File(File(context.filesDir, "atomcode"), MARKER)
}
