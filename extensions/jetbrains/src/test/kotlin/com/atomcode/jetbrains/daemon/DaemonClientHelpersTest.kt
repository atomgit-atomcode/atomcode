package com.atomcode.jetbrains.daemon

import kotlin.test.Test
import kotlin.test.assertEquals

class DaemonClientHelpersTest {

    @Test
    fun `jsonQuoted wraps empty string`() {
        assertEquals("\"\"", "".jsonQuoted())
    }

    @Test
    fun `jsonQuoted wraps normal string`() {
        assertEquals("\"hello\"", "hello".jsonQuoted())
    }

    @Test
    fun `jsonQuoted escapes backslashes`() {
        assertEquals("\"a\\\\b\"", "a\\b".jsonQuoted())
    }

    @Test
    fun `jsonQuoted escapes double quotes`() {
        assertEquals("\"say \\\"hello\\\"\"", "say \"hello\"".jsonQuoted())
    }

    @Test
    fun `jsonQuoted escapes newlines`() {
        assertEquals("\"line1\\nline2\"", "line1\nline2".jsonQuoted())
    }

    @Test
    fun `jsonQuoted handles mixed special characters`() {
        assertEquals("\"a\\\\b \\\"c\\\"\\nd\"", "a\\b \"c\"\nd".jsonQuoted())
    }

    @Test
    fun `jsonQuotedOrNull returns null for null input`() {
        val input: String? = null
        assertEquals("null", input.jsonQuotedOrNull())
    }

    @Test
    fun `jsonQuotedOrNull delegates to jsonQuoted for non-null`() {
        assertEquals("\"test\"", "test".jsonQuotedOrNull())
    }

    @Test
    fun `urlPathEncoded handles empty string`() {
        assertEquals("", "".urlPathEncoded())
    }

    @Test
    fun `urlPathEncoded encodes spaces`() {
        assertEquals("hello%20world", "hello world".urlPathEncoded())
    }

    @Test
    fun `urlPathEncoded encodes special characters`() {
        assertEquals("a%2Fb%3Fc%3Dd", "a/b?c=d".urlPathEncoded())
    }

    @Test
    fun `urlPathEncoded encodes unicode`() {
        assertEquals("%E4%BD%A0%E5%A5%BD", "你好".urlPathEncoded())
    }

    @Test
    fun `urlPathEncoded replaces plus with percent20`() {
        assertEquals("a%2Bb", "a+b".urlPathEncoded())
    }

    @Test
    fun `urlPathEncoded encodes slashes`() {
        assertEquals("path%2Fto%2Ffile", "path/to/file".urlPathEncoded())
    }
}
