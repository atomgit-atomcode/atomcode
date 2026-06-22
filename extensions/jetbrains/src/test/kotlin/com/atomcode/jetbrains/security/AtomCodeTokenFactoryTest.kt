package com.atomcode.jetbrains.security

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class AtomCodeTokenFactoryTest {

    @Test
    fun `createToken returns a 43 character string`() {
        val token = AtomCodeTokenFactory.createToken()
        assertEquals(43, token.length)
    }

    @Test
    fun `createToken contains no padding characters`() {
        val token = AtomCodeTokenFactory.createToken()
        assertFalse('=' in token)
    }

    @Test
    fun `createToken contains only URL-safe Base64 characters`() {
        val validChars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        val token = AtomCodeTokenFactory.createToken()
        assertTrue(token.all { it in validChars })
    }

    @Test
    fun `createToken produces different values on subsequent calls`() {
        val token1 = AtomCodeTokenFactory.createToken()
        val token2 = AtomCodeTokenFactory.createToken()
        assertFalse(token1 == token2)
    }

    @Test
    fun `createToken does not contain URL-unsafe characters`() {
        val token = AtomCodeTokenFactory.createToken()
        assertFalse('/' in token)
        assertFalse('+' in token)
    }
}
