package com.atomcode.jetbrains.ui

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

class ChatPanelHelpersTest {

    @Test
    fun `decodeHistoryUserMessage preserves a plain prompt`() {
        assertEquals(
            HistoryUserMessage("How does this work?", emptyList()),
            decodeHistoryUserMessage("How does this work?"),
        )
    }

    @Test
    fun `decodeHistoryUserMessage restores prompt and attachment names`() {
        val stored = """The user has attached the following file(s)/selection(s) for context. The content is provided inline below - DO NOT use read_file to re-read them.

File: src/main.kt (lines 4-9)
```kotlin
fun main() = println("hello")
```

File: README.md
```markdown
# Example
```

User question: Review these files
and suggest improvements."""

        assertEquals(
            HistoryUserMessage(
                text = "Review these files\nand suggest improvements.",
                contextSummary = listOf("src/main.kt", "README.md"),
            ),
            decodeHistoryUserMessage(stored),
        )
    }

    @Test
    fun `summarizeToolArguments extracts bash command`() {
        assertEquals(
            "cargo test --workspace",
            summarizeToolArguments("bash", """{"command":"cargo test --workspace"}"""),
        )
    }

    @Test
    fun `summarizeToolArguments collapses command whitespace`() {
        assertEquals(
            "git status --short",
            summarizeToolArguments("bash", """{"command":"git status\n  --short"}"""),
        )
    }

    @Test
    fun `summarizeToolArguments omits unknown tool arguments`() {
        assertEquals("", summarizeToolArguments("unknown", """{"token":"secret"}"""))
    }

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
    fun `slashPromptTemplate transforms slash review`() {
        assertEquals(
            "Please review this code for issues, improvements, and best practices.",
            slashPromptTemplate("/review"),
        )
    }

    @Test
    fun `slashPromptTemplate appends suffix when present`() {
        val result = slashPromptTemplate("/review some specific code")
        assertEquals(
            "Please review this code for issues, improvements, and best practices.\n\nsome specific code",
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
        val result = slashPromptTemplate("/review   \n  multiple words\nhere  \n")
        assertEquals(
            "Please review this code for issues, improvements, and best practices.\n\nmultiple words\nhere",
            result,
        )
    }

    @Test
    fun `slashPromptTemplate is case insensitive for commands`() {
        assertEquals(
            "Please review this code for issues, improvements, and best practices.",
            slashPromptTemplate("/REVIEW"),
        )
    }
}
