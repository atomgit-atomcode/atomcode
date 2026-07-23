package com.atomcode.jetbrains.daemon

import com.google.gson.JsonParser
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
    fun `jsonQuoted escapes all JSON control characters`() {
        val input = "tab:\t cr:\r backspace:\b formfeed:\u000C nul:\u0000"

        val quoted = input.jsonQuoted()

        assertEquals("\"tab:\\t cr:\\r backspace:\\b formfeed:\\f nul:\\u0000\"", quoted)
        assertEquals(input, JsonParser.parseString(quoted).asString)
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
    fun `formatDaemonHttpError uses json error field`() {
        val message = formatDaemonHttpError(400, """{"error":"bad request"}""")

        assertEquals("Daemon request failed: HTTP 400: bad request", message)
    }

    @Test
    fun `formatDaemonHttpError uses json message field`() {
        val message = formatDaemonHttpError(413, """{"message":"Failed to buffer the request body"}""")

        assertEquals("Daemon request failed: HTTP 413: Failed to buffer the request body", message)
    }

    @Test
    fun `formatDaemonHttpError unwraps json string bodies`() {
        val message = formatDaemonHttpError(404, """"Session not found"""")

        assertEquals("Daemon request failed: HTTP 404: Session not found", message)
    }

    @Test
    fun `formatDaemonHttpError trims raw html bodies`() {
        val html = "<!DOCTYPE html>" + "x".repeat(600)
        val prefix = "Daemon request failed: HTTP 404: "

        val message = formatDaemonHttpError(404, html)

        assertEquals(550, message.length)
        assertEquals(prefix + html.take(550 - prefix.length - 3) + "...", message)
    }

    @Test
    fun `daemon HTTP exception preserves structured retry metadata`() {
        val error = DaemonHttpException.from(
            503,
            """{"error":"temporarily unavailable","code":"login_poll_unavailable","retryable":true}""",
        )

        assertEquals(503, error.statusCode)
        assertEquals("login_poll_unavailable", error.code)
        assertEquals(true, error.retryable)
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

    @Test
    fun `urlQueryEncoded keeps spaces as plus for query strings`() {
        assertEquals("hello+world", "hello world".urlQueryEncoded())
    }

    @Test
    fun `parseSessionMetaList parses ordinary session list`() {
        val sessions = parseSessionMetaList(
            """
            [
              {"project_hash":"hash-1","id":"s1","name":"One","updated_at":10,"message_count":2}
            ]
            """.trimIndent(),
        )

        assertEquals(1, sessions.size)
        assertEquals("s1", sessions.single().id)
        assertEquals("hash-1", sessions.single().projectHash)
        assertEquals(10L, sessions.single().updatedAt)
        assertEquals(2, sessions.single().messageCount)
    }

    @Test
    fun `parseSessionMetaList parses search result wrapper`() {
        val sessions = parseSessionMetaList(
            """
            [
              {
                "project_hash":"hash-2",
                "meta":{"id":"s2","name":"Two","updated_at":20,"message_count":3}
              }
            ]
            """.trimIndent(),
        )

        assertEquals(1, sessions.size)
        assertEquals("s2", sessions.single().id)
        assertEquals("hash-2", sessions.single().projectHash)
        assertEquals(20L, sessions.single().updatedAt)
        assertEquals(3, sessions.single().messageCount)
    }

    @Test
    fun `parseMessageInfo reads snake case internal origin`() {
        val message = parseMessageInfo(
            """{"role":"assistant","content":"No verification is needed.","internal_origin":"verify_cadence"}""",
        )

        assertEquals("assistant", message.role)
        assertEquals("verify_cadence", message.internalOrigin)
    }

    @Test
    fun `parseMessageInfo reads camel case internal origin`() {
        val message = parseMessageInfo(
            """{"role":"assistant","content":"No verification is needed.","internalOrigin":"verify_cadence"}""",
        )

        assertEquals("verify_cadence", message.internalOrigin)
    }

    @Test
    fun `parseMessageInfo reads tool calls`() {
        val message = parseMessageInfo(
            """{"role":"assistant","content":"","tool_calls":[{"id":"t1","name":"bash","arguments":"{\"command\":\"true\"}"}]}""",
        )

        assertEquals(1, message.toolCalls.size)
        assertEquals("t1", message.toolCalls.single().id)
        assertEquals("bash", message.toolCalls.single().name)
        assertEquals("""{"command":"true"}""", message.toolCalls.single().arguments)
    }
}
