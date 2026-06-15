package com.atomcode.jetbrains.files

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class FileChangeServiceTest {

    @Test
    fun `parsePorcelainPath parses staged modified file`() {
        assertEquals("src/main.kt", parsePorcelainPath("M  src/main.kt"))
    }

    @Test
    fun `parsePorcelainPath parses unstaged modified file`() {
        assertEquals("src/main.kt", parsePorcelainPath(" M src/main.kt"))
    }

    @Test
    fun `parsePorcelainPath parses untracked file`() {
        assertEquals("newfile.txt", parsePorcelainPath("?? newfile.txt"))
    }

    @Test
    fun `parsePorcelainPath parses added file`() {
        assertEquals("added.txt", parsePorcelainPath("A  added.txt"))
    }

    @Test
    fun `parsePorcelainPath parses renamed file arrow syntax`() {
        assertEquals("new.txt", parsePorcelainPath("R  old.txt -> new.txt"))
    }

    @Test
    fun `parsePorcelainPath handles quoted path in rename`() {
        assertEquals("new name.txt", parsePorcelainPath("R  \"old name.txt\" -> \"new name.txt\""))
    }

    @Test
    fun `parsePorcelainPath returns null for short line`() {
        assertNull(parsePorcelainPath("ab"))
    }

    @Test
    fun `parsePorcelainPath returns null for blank raw portion after trim`() {
        assertNull(parsePorcelainPath("XY  "))
    }

    @Test
    fun `parsePorcelainPath trims surrounding whitespace`() {
        assertEquals("src/main.kt", parsePorcelainPath(" M   src/main.kt   "))
    }
}
