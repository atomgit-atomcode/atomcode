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

    private fun isWindows(): Boolean =
        System.getProperty("os.name").lowercase().contains("win")
}
