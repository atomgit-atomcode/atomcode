package com.atomcode.jetbrains.security

import kotlin.test.Test
import kotlin.test.assertEquals

class SecretRedactorTest {

    @Test
    fun `redact handles empty string`() {
        assertEquals("", SecretRedactor.redact(""))
    }

    @Test
    fun `redact passes through non-sensitive text`() {
        val input = "Hello world, this is a normal message."
        assertEquals(input, SecretRedactor.redact(input))
    }

    @Test
    fun `redact redacts bearer auth header`() {
        val result = SecretRedactor.redact("Authorization: Bearer sk-secret-abc123")
        assertEquals("Authorization: Bearer [REDACTED]", result)
    }

    @Test
    fun `redact is case insensitive for authorization header`() {
        val result = SecretRedactor.redact("authorization: bearer abc123")
        assertEquals("authorization: bearer [REDACTED]", result)
    }

    @Test
    fun `redact redacts api_key with colon delimiter`() {
        val result = SecretRedactor.redact("api_key: sk-secret-abc123")
        assertEquals("api_key: [REDACTED]", result)
    }

    @Test
    fun `redact redacts api_key with equals delimiter`() {
        val result = SecretRedactor.redact("api_key=sk-secret-abc123")
        assertEquals("api_key=[REDACTED]", result)
    }

    @Test
    fun `redact redacts api_key with quoted value`() {
        val result = SecretRedactor.redact("api_key\" : \"sk-secret-abc123\"")
        assertEquals("api_key\" : \"[REDACTED]\"", result)
    }

    @Test
    fun `redact redacts api_key with single quote delimiter`() {
        val result = SecretRedactor.redact("api_key' = 'abc123'")
        assertEquals("api_key' = '[REDACTED]'", result)
    }

    @Test
    fun `redact redacts API_KEY underscore variant`() {
        val result = SecretRedactor.redact("API_KEY=sk-secret-abc123")
        assertEquals("API_KEY=[REDACTED]", result)
    }

    @Test
    fun `redact redacts token with colon delimiter`() {
        val result = SecretRedactor.redact("token: my-secret-token-xyz")
        assertEquals("token: [REDACTED]", result)
    }

    @Test
    fun `redact redacts token with equals delimiter`() {
        val result = SecretRedactor.redact("token = my-secret-token-xyz")
        assertEquals("token = [REDACTED]", result)
    }

    @Test
    fun `redact redacts token with quoted value`() {
        val result = SecretRedactor.redact("token\": \"my-secret-token-xyz\"")
        assertEquals("token\": \"[REDACTED]\"", result)
    }

    @Test
    fun `redact handles multiple secrets in one string`() {
        val input = "Authorization: Bearer tok1 and api_key: secret2"
        val result = SecretRedactor.redact(input)
        assertEquals("Authorization: Bearer [REDACTED] and api_key: [REDACTED]", result)
    }

    @Test
    fun `redact redacts secret_key`() {
        val result = SecretRedactor.redact("secret_key: abc123")
        assertEquals("secret_key: [REDACTED]", result)
    }

    @Test
    fun `redact redacts access_key`() {
        val result = SecretRedactor.redact("access_key = AKIAIOSFODNN7EXAMPLE")
        assertEquals("access_key = [REDACTED]", result)
    }

    @Test
    fun `redact redacts auth_token`() {
        val result = SecretRedactor.redact("auth_token: ghp_1234567890abcdef")
        assertEquals("auth_token: [REDACTED]", result)
    }

    @Test
    fun `redact redacts client_secret`() {
        val result = SecretRedactor.redact("client_secret=\"my-secret\"")
        assertEquals("client_secret=\"[REDACTED]\"", result)
    }

    @Test
    fun `redact redacts private_key`() {
        val result = SecretRedactor.redact("private_key: abcdef123456")
        assertEquals("private_key: [REDACTED]", result)
    }

    @Test
    fun `redact redacts password field`() {
        val result = SecretRedactor.redact("password: hunter2")
        assertEquals("password: [REDACTED]", result)
    }

    @Test
    fun `redact redacts credential field`() {
        val result = SecretRedactor.redact("credential = top-secret")
        assertEquals("credential = [REDACTED]", result)
    }
}
