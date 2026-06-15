package com.atomcode.jetbrains.ui

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class ChatPanelHelpersTest {

    @Test
    fun `extractLastCodeBlock returns null for text without code blocks`() {
        assertNull(extractLastCodeBlock("Hello world"))
    }

    @Test
    fun `extractLastCodeBlock extracts a single code block`() {
        val result = extractLastCodeBlock("```kotlin\nval x = 1\n```")
        assertEquals("val x = 1", result)
    }

    @Test
    fun `extractLastCodeBlock returns the last of multiple blocks`() {
        val text = """```python
print("first")
```
Some text in between
```kotlin
val x = 1
```"""
        val result = extractLastCodeBlock(text)
        assertEquals("val x = 1", result)
    }

    @Test
    fun `extractLastCodeBlock handles an empty code block`() {
        val result = extractLastCodeBlock("```\n```")
        assertEquals("", result)
    }

    @Test
    fun `extractLastCodeBlock preserves the language specifier in match but not in content`() {
        val result = extractLastCodeBlock("""```python
def hello():
    pass
```""")
        assertEquals("def hello():\n    pass", result)
    }

    @Test
    fun `extractLastCodeBlock trims trailing whitespace from the extracted block`() {
        val result = extractLastCodeBlock("""```json
{"key": "value"}

```""")
        assertEquals("{\"key\": \"value\"}", result)
    }

    // --- slashPromptTemplate ---

    @Test
    fun `slashPromptTemplate transforms slash explain`() {
        assertEquals(
            "Please explain this code. What does it do, and why?",
            slashPromptTemplate("/explain"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash fix`() {
        assertEquals(
            "Please fix any bugs or issues in this code.",
            slashPromptTemplate("/fix"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash test`() {
        assertEquals(
            "Please write tests for this code.",
            slashPromptTemplate("/test"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash refactor`() {
        assertEquals(
            "Please refactor this code for better readability and maintainability.",
            slashPromptTemplate("/refactor"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash docs`() {
        assertEquals(
            "Please add documentation comments to this code.",
            slashPromptTemplate("/docs"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash review`() {
        assertEquals(
            "Please review this code for issues, improvements, and best practices.",
            slashPromptTemplate("/review"),
        )
    }

    @Test
    fun `slashPromptTemplate transforms slash optimize`() {
        assertEquals(
            "Please optimize this code for better performance and readability.",
            slashPromptTemplate("/optimize"),
        )
    }

    @Test
    fun `slashPromptTemplate appends suffix when present`() {
        val result = slashPromptTemplate("/explain some specific code")
        assertEquals(
            "Please explain this code. What does it do, and why?\n\nsome specific code",
            result,
        )
    }

    @Test
    fun `slashPromptTemplate returns null for unknown command`() {
        assertNull(slashPromptTemplate("/unknown"))
    }

    @Test
    fun `slashPromptTemplate returns null for non-command input`() {
        assertNull(slashPromptTemplate("just a normal message"))
    }

    @Test
    fun `slashPromptTemplate handles suffix with extra whitespace`() {
        val result = slashPromptTemplate("/test   \n  multiple words\nhere  \n")
        assertEquals(
            "Please write tests for this code.\n\nmultiple words\nhere",
            result,
        )
    }

    @Test
    fun `slashPromptTemplate is case insensitive for commands`() {
        assertEquals(
            "Please explain this code. What does it do, and why?",
            slashPromptTemplate("/EXPLAIN"),
        )
    }
}
