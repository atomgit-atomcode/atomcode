package com.atomcode.app

import android.content.Context
import android.util.Log
import java.io.File
import java.io.IOException

/**
 * Start the bundled CLI as a real `atomcode daemon`, from inside the app.
 *
 * Why an external daemon at all: the in-app runtime and a daemon started by
 * hand (`adb shell`, `run-as`) cannot both own a session — the lease keeps them
 * apart. The external one is what you want when the phone is also driven over
 * `adb`, or when you want the runtime to outlive the UI process.
 *
 * Why it can start one at all: Android only executes binaries from an app's
 * `nativeLibraryDir`, and that is exactly where the helper APK puts the CLI
 * (`libatomcode.so`, linked as `<data>/bin/atomcode` by `shims`).
 *
 * It is given the **same product directory** as the app, so it reads the same
 * `config.toml` and `auth.toml` (no credential copying) and writes its handoff
 * file where `ExternalDaemon` looks for it. Its working directory is the shared
 * workspace, which puts its session root on the same `.sessions` the app uses —
 * one history, not two.
 */
object ExternalDaemonLauncher {
    private const val TAG = "AtomCode"

    /** The port the launched daemon listens on. Fixed so the app can find it. */
    const val PORT = 8787

    /** How long to wait for the daemon to answer before calling it a failure. */
    private const val READY_TIMEOUT_MS = 20_000L

    @Volatile
    private var process: Process? = null

    /** Whether a daemon we started is still alive. */
    fun isRunning(): Boolean = process?.isAlive == true

    /**
     * Start the daemon and wait until it answers `/health`.
     *
     * Returns null on success, or a human-readable reason. Runs off the UI
     * thread — call it from a worker.
     */
    fun start(context: Context): String? {
        if (isRunning()) return null

        val dataDir = File(context.filesDir, "atomcode")
        val binary = File(dataDir, "bin/atomcode")
        if (!binary.exists()) {
            return "helper not installed: ${binary.absolutePath} is missing"
        }
        // The workspace has to exist before the daemon uses it as its cwd.
        val workspace = File(
            android.os.Environment.getExternalStorageDirectory(),
            "AtomCode",
        ).apply { mkdirs() }
        val logFile = File(dataDir, "external-daemon.log")

        val builder = ProcessBuilder(binary.absolutePath, "daemon", "--port", PORT.toString())
        builder.directory(workspace)
        builder.redirectErrorStream(true)
        builder.redirectOutput(ProcessBuilder.Redirect.appendTo(logFile))
        // Same product tree as the app: shared config, shared credentials, and
        // the handoff file lands where discovery looks for it.
        builder.environment()["ATOMCODE_HOME"] = dataDir.absolutePath

        val started = try {
            builder.start()
        } catch (failure: IOException) {
            Log.w(TAG, "cannot start the external daemon", failure)
            return "cannot execute ${binary.absolutePath}: ${failure.message}"
        }
        process = started

        // Reading the log is the only way to say *why* it never answered, so the
        // failure message carries the tail instead of a bare timeout.
        val deadline = System.currentTimeMillis() + READY_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            if (ExternalDaemon.isAlive(PORT)) {
                Log.i(TAG, "external daemon answering on port $PORT")
                return null
            }
            if (!started.isAlive) {
                process = null
                return "the daemon exited immediately: ${logTail(logFile)}"
            }
            try {
                Thread.sleep(250)
            } catch (interrupted: InterruptedException) {
                Thread.currentThread().interrupt()
                break
            }
        }
        stop()
        return "the daemon did not answer within ${READY_TIMEOUT_MS / 1000}s (see ${logFile.name})"
    }

    /** Stop the daemon we started, if any. */
    fun stop() {
        val running = process ?: return
        process = null
        running.destroy()
        try {
            if (!running.waitFor(3, java.util.concurrent.TimeUnit.SECONDS)) running.destroyForcibly()
        } catch (interrupted: InterruptedException) {
            Thread.currentThread().interrupt()
            running.destroyForcibly()
        }
        Log.i(TAG, "external daemon stopped")
    }

    private fun logTail(file: File): String = try {
        val lines = file.readLines()
        lines.takeLast(3).joinToString(" / ")
    } catch (failure: Exception) {
        "(no log)"
    }
}
