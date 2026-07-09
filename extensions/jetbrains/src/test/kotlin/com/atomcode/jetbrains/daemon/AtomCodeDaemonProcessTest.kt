package com.atomcode.jetbrains.daemon

import com.atomcode.jetbrains.settings.AtomCodeSettings
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

    private fun isWindows(): Boolean =
        System.getProperty("os.name").lowercase().contains("win")
}
