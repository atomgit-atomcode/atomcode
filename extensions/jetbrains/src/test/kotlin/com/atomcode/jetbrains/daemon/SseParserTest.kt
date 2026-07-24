package com.atomcode.jetbrains.daemon

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class SseParserTest {
    @Test
    fun parsesRuntimeInfoEvent() {
        val parser = SseParser()
        val events = parser.feed(
            """data: {"type":"runtime_info","provider":"main","model":"model-x"}${"\n\n"}""",
        )
        assertEquals(ChatEvent.RuntimeInfo("main", "model-x"), events.single())
    }

    @Test
    fun parsesTextEvent() {
        val parser = SseParser()
        val events = parser.feed("""data: {"type":"text","content":"hello"}${"\n\n"}""")
        assertEquals(listOf(ChatEvent.Text("hello")), events)
    }

    @Test
    fun parsesMultilineData() {
        val parser = SseParser()
        val events = parser.feed("data: {\"type\":\"text\",\ndata: \"content\":\"hello\"}\n\n")
        assertEquals(listOf(ChatEvent.Text("hello")), events)
    }

    @Test
    fun unknownEventDoesNotCrash() {
        val parser = SseParser()
        val events = parser.feed("""data: {"type":"new_event"}${"\n\n"}""")
        assertTrue(events.single() is ChatEvent.Unknown)
    }

    @Test
    fun parsesWarningEvent() {
        val parser = SseParser()
        val events = parser.feed("""data: {"type":"warning","message":"current model does not support image input"}${"\n\n"}""")
        assertEquals(
            listOf(ChatEvent.Warning("current model does not support image input")),
            events,
        )
    }

    @Test
    fun parsesToolProgressEvent() {
        val parser = SseParser()
        val events = parser.feed(
            """data: {"type":"tool_progress","id":"call-1","progress":"review · round 2 · thinking"}${"\n\n"}""",
        )

        assertEquals(
            listOf(ChatEvent.ToolProgress("call-1", "review · round 2 · thinking")),
            events,
        )
    }

    @Test
    fun parsesPermissionRequest() {
        val parser = SseParser()
        val events = parser.feed(
            """data: {"type":"permission_request","session_id":"s1","tool_name":"mcp__repo__edit","reason":"Modify file","call_id":"c1","arguments":"{\"path\":\"README.md\"}"}${"\n\n"}""",
        )

        assertEquals(
            ChatEvent.PermissionRequest(
                sessionId = "s1",
                toolName = "mcp__repo__edit",
                reason = "Modify file",
                callId = "c1",
                arguments = "{\"path\":\"README.md\"}",
            ),
            events.single(),
        )
    }

    @Test
    fun parsesLargeToolResultWithoutRecursiveRegexOverflow() {
        val parser = SseParser()
        val output = buildString {
            repeat(80_000) {
                append("line ")
                append(it)
                append(" \\\"quoted\\\" \\\\ path\\n")
            }
        }

        val events = parser.feed(
            """data: {"type":"tool_result","id":"call-1","name":"bash","output":"$output","success":true,"duration_ms":123}${"\n\n"}""",
        )

        assertEquals(
            ChatEvent.ToolResult(
                id = "call-1",
                name = "bash",
                output = output.replace("\\\"", "\"").replace("\\\\", "\\").replace("\\n", "\n"),
                success = true,
                durationMs = 123,
            ),
            events.single(),
        )
    }

    @Test
    fun parsesJsonObjectArray() {
        val objects = """[{"id":"s1","name":"One"},{"id":"s2","name":"Two","nested":{"ok":true}}]""".jsonObjects()

        assertEquals(2, objects.size)
        assertEquals("s1", objects[0].jsonString("id"))
        assertEquals("Two", objects[1].jsonString("name"))
    }

    @Test
    fun parsesNamedObjectArrayAndUnescapesContent() {
        val raw = """{"messages":[{"role":"user","content":"hello\nworld"},{"role":"assistant","content":"{\"ok\":true}"}]}"""
        val messages = raw.jsonArrayObjects("messages")

        assertEquals(2, messages.size)
        assertEquals("hello\nworld", messages[0].jsonString("content"))
        assertEquals("{\"ok\":true}", messages[1].jsonString("content"))
    }

    @Test
    fun parsesNestedObject() {
        val raw = """{"logged_in":true,"user":{"username":"alice","name":"Alice"},"token":null}"""
        val user = raw.jsonNestedObject("user")

        assertEquals("Alice", user?.jsonString("name"))
        assertEquals("alice", user?.jsonString("username"))
    }

    @Test
    fun parsesProviderAndModelPayloads() {
        val providers = """{"default_provider":"main","providers":[{"name":"main","type":"openai","model":"gpt-x","is_default":true,"has_api_key":true}]}"""
        val models = """[{"provider":"main","model":"gpt-x","provider_type":"openai","is_default":true}]"""

        assertEquals("main", providers.jsonString("default_provider"))
        assertEquals("gpt-x", providers.jsonArrayObjects("providers").single().jsonString("model"))
        assertEquals("main", models.jsonObjects().single().jsonString("provider"))
    }

    @Test
    fun parsesDaemonSmokeAuthPayloadShape() {
        val raw = """
            {
              "logged_in": true,
              "auth_path": "/Users/example/.atomcode/auth.toml",
              "user": {
                "id": "u1",
                "username": "danmingzhen",
                "name": "\u6253\u7801",
                "email": "user@example.com",
                "avatar_url": "https://example.com/avatar.jpg"
              },
              "token": {
                "token_type": "Bearer",
                "expires_in": 604800,
                "created_at": 1780902425,
                "has_refresh_token": true
              }
            }
        """.trimIndent()

        val user = raw.jsonNestedObject("user")
        val token = raw.jsonNestedObject("token")

        assertEquals(true, raw.jsonBoolean("logged_in"))
        assertEquals("/Users/example/.atomcode/auth.toml", raw.jsonString("auth_path"))
        assertEquals("打码", user?.jsonString("name"))
        assertEquals("danmingzhen", user?.jsonString("username"))
        assertEquals(604800, token?.jsonInt("expires_in"))
        assertEquals(true, token?.jsonBoolean("has_refresh_token"))
    }

    @Test
    fun parsesDaemonSmokeProvidersPayloadShape() {
        val raw = """
            {
              "default_provider": "AtomGit-deepseek-v4-flash",
              "providers": [
                {
                  "base_url": "https://api.deepseek.com/v1",
                  "context_window": 1000000,
                  "ephemeral": false,
                  "has_api_key": true,
                  "is_default": false,
                  "max_tokens": null,
                  "model": "deepseek-v4-pro",
                  "name": "agentgate",
                  "reasoning_effort": null,
                  "thinking_budget": null,
                  "thinking_enabled": null,
                  "thinking_keep": null,
                  "thinking_type": null,
                  "type": "openai"
                },
                {
                  "base_url": "https://llm-api.atomgit.com/v1",
                  "context_window": 1000000,
                  "ephemeral": false,
                  "has_api_key": false,
                  "is_default": true,
                  "max_tokens": null,
                  "model": "deepseek-v4-flash",
                  "name": "AtomGit-deepseek-v4-flash",
                  "reasoning_effort": "max",
                  "thinking_budget": null,
                  "thinking_enabled": null,
                  "thinking_keep": null,
                  "thinking_type": null,
                  "type": "openai"
                }
              ]
            }
        """.trimIndent()

        val providers = raw.jsonArrayObjects("providers")

        assertEquals("AtomGit-deepseek-v4-flash", raw.jsonString("default_provider"))
        assertEquals(2, providers.size)
        assertEquals("agentgate", providers[0].jsonString("name"))
        assertEquals(true, providers[0].jsonBoolean("has_api_key"))
        assertEquals(false, providers[0].jsonBoolean("is_default"))
        assertEquals("AtomGit-deepseek-v4-flash", providers[1].jsonString("name"))
        assertEquals("deepseek-v4-flash", providers[1].jsonString("model"))
        assertEquals(true, providers[1].jsonBoolean("is_default"))
        assertEquals(null, providers[1].jsonBoolean("thinking_enabled"))
        assertEquals(null, providers[1].jsonInt("thinking_budget"))
    }

    @Test
    fun parsesDaemonSmokeModelsAndSessionsPayloadShapes() {
        val models = """
            [
              {"provider":"agentgate","model":"deepseek-v4-pro","provider_type":"openai","is_default":false,"effort_applicable":true,"reasoning_effort":null},
              {"provider":"AtomGit-deepseek-v4-flash","model":"deepseek-v4-flash","provider_type":"openai","is_default":true,"effort_applicable":true,"reasoning_effort":"max"}
            ]
        """.trimIndent()
        val sessions = """
            [
              {
                "project_hash": "4cd349a275768311",
                "id": "04a07cdb-7958-4c78-99b3-548c9c9d3f6b",
                "name": "\u603b\u7ed3\u4fee\u6539\u7684\u5185\u5bb9\uff0c\u63d0\u4ea4\u4ee3\u7801",
                "working_dir": "/Users/example/atomcode",
                "created_at": 1781187197,
                "updated_at": 1781187246,
                "message_count": 8,
                "file_size": 5172
              }
            ]
        """.trimIndent()

        val modelObjects = models.jsonObjects()
        val sessionObjects = sessions.jsonObjects()

        assertEquals(2, modelObjects.size)
        assertEquals("deepseek-v4-flash", modelObjects[1].jsonString("model"))
        assertEquals(true, modelObjects[1].jsonBoolean("is_default"))
        assertEquals("04a07cdb-7958-4c78-99b3-548c9c9d3f6b", sessionObjects.single().jsonString("id"))
        assertEquals("总结修改的内容，提交代码", sessionObjects.single().jsonString("name"))
        assertEquals(1781187246L, sessionObjects.single().jsonLong("updated_at"))
        assertEquals(8, sessionObjects.single().jsonInt("message_count"))
    }
}
