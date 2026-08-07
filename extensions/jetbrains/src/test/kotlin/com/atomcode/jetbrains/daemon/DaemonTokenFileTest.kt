package com.atomcode.jetbrains.daemon

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test
import java.nio.file.Files

class DaemonTokenFileTest {
    @Test
    fun readsTokenFromFile() {
        val home = Files.createTempDirectory("ac-jb").toFile()
        System.setProperty("user.home", home.absolutePath)
        val f = home.resolve(".atomcode").apply { mkdirs() }.resolve("daemon-13456.json")
        f.writeText("""{"pid":1,"port":13456,"token":"jb-tok"}""")
        assertEquals("jb-tok", DaemonTokenFile.read(13456))
    }

    @Test
    fun returnsNullWhenMissing() {
        val home = Files.createTempDirectory("ac-jb2").toFile()
        System.setProperty("user.home", home.absolutePath)
        assertNull(DaemonTokenFile.read(19999))
    }

    @Test
    fun returnsNullOnInvalidJson() {
        val home = Files.createTempDirectory("ac-jb4").toFile()
        System.setProperty("user.home", home.absolutePath)
        val f = home.resolve(".atomcode").apply { mkdirs() }.resolve("daemon-13456.json")
        f.writeText("not-valid-json")
        assertNull(DaemonTokenFile.read(13456))
    }

    @Test
    fun returnsNullWhenTokenFieldMissing() {
        val home = Files.createTempDirectory("ac-jb5").toFile()
        System.setProperty("user.home", home.absolutePath)
        val f = home.resolve(".atomcode").apply { mkdirs() }.resolve("daemon-13456.json")
        f.writeText("""{"pid":1,"port":13456}""")
        assertNull(DaemonTokenFile.read(13456))
    }
}
