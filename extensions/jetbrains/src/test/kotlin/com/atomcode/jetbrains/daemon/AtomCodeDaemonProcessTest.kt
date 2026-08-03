package com.atomcode.jetbrains.daemon

import com.atomcode.jetbrains.settings.AtomCodeSettings
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class AtomCodeDaemonProcessTest {
    @Test
    fun locatesBundledDaemonResource() {
        val resolution = AtomCodeDaemonProcess(AtomCodeSettings()).locateBinary()

        assertNotNull(resolution)
        assertEquals(emptyList(), resolution.argsPrefix)
        assertTrue(resolution.path.contains("atomcode-jetbrains"))
        assertTrue(resolution.path.endsWith(if (isWindows()) "atomcode-daemon.exe" else "atomcode-daemon"))
    }

    @Test
    fun readsExpectedBundledVersion() {
        val version = AtomCodeDaemonProcess(AtomCodeSettings()).expectedBundledVersion()

        assertNotNull(version)
        assertTrue(version.matches(Regex("""\d+\.\d+\.\d+.*""")))
    }

    @Test
    fun extractsBundledDaemonToContentAddressedPath() {
        val daemon = AtomCodeDaemonProcess(AtomCodeSettings())
        val expectedHash = assertNotNull(daemon.expectedBundledHash())

        val resolution = assertNotNull(daemon.locateBinary())

        assertTrue(
            resolution.path.contains(expectedHash.take(16)),
            "bundled daemon path must change with its content so a running Windows executable is never overwritten",
        )
    }

    @Test
    fun normalizeDaemonEnvForUtf8LocaleReplacesCLocale() {
        if (isWindows()) return
        val env = mutableMapOf(
            "LC_ALL" to "C",
            "LANG" to "C",
        )

        normalizeDaemonEnvForUtf8Locale(env)

        assertTrue(env.values.any { it.contains("utf", ignoreCase = true) })
    }

    @Test
    fun normalizeDaemonEnvForUtf8LocalePreservesExistingUtf8Locale() {
        val env = mutableMapOf(
            "LC_ALL" to "zh_CN.UTF-8",
            "LANG" to "zh_CN.UTF-8",
        )

        normalizeDaemonEnvForUtf8Locale(env)

        assertEquals("zh_CN.UTF-8", env["LC_ALL"])
        assertEquals("zh_CN.UTF-8", env["LANG"])
    }

    @Test
    fun normalizeDaemonEnvForUtf8LocaleReplacesCCtypeEvenWhenLangMentionsUtf8() {
        if (isWindows()) return
        val env = mutableMapOf(
            "LC_CTYPE" to "C",
            "LANG" to "UTF-8",
        )

        normalizeDaemonEnvForUtf8Locale(env)

        assertTrue(env["LC_CTYPE"]?.contains("utf", ignoreCase = true) == true)
    }

    @Test
    fun normalizeDaemonEnvForUtf8LocaleReplacesBareUtf8CtypeOnLinux() {
        val env = mutableMapOf(
            "LC_CTYPE" to "UTF-8",
            "LANG" to "C",
        )

        normalizeDaemonEnvForUtf8Locale(env, "linux")

        assertEquals("C.UTF-8", env["LC_CTYPE"])
    }

    @Test
    fun normalizeDaemonEnvForUtf8LocalePreservesBareUtf8CtypeOnMac() {
        val env = mutableMapOf(
            "LC_CTYPE" to "UTF-8",
            "LANG" to "C",
        )

        normalizeDaemonEnvForUtf8Locale(env, "Mac OS X")

        assertEquals("UTF-8", env["LC_CTYPE"])
    }

    @Test
    fun processExitRemainsObservableWhenStderrReadFails() {
        val exit = JvmManagedDaemonProcess(BrokenStderrProcess()).onExit().get(1, TimeUnit.SECONDS)

        assertEquals(7, exit.exitCode)
        assertEquals("", exit.stderr)
    }

    private fun isWindows(): Boolean =
        System.getProperty("os.name").lowercase().contains("win")
}

private class BrokenStderrProcess : Process() {
    override fun getOutputStream(): OutputStream = ByteArrayOutputStream()
    override fun getInputStream(): InputStream = ByteArrayInputStream(byteArrayOf())
    override fun getErrorStream(): InputStream = object : InputStream() {
        override fun read(): Int = throw IOException("stderr pipe closed")
    }
    override fun waitFor(): Int = 7
    override fun exitValue(): Int = 7
    override fun destroy() = Unit
    override fun isAlive(): Boolean = false
    override fun onExit(): CompletableFuture<Process> = CompletableFuture.completedFuture(this)
}
