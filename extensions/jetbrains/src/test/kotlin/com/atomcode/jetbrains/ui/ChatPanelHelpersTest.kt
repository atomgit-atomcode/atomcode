package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.ui.input.slashCommandPrefix
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ChatPanelHelpersTest {
    private val reviewTemplate = "请审查这段代码，重点关注潜在问题、改进建议和最佳实践。"


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
    fun `extractLastCodeBlock handles longer fence with inner backticks`() {
        val markdown = fencedArtifactMarkdown("kotlin", "val fence = \"```\"")
        assertEquals("val fence = \"```\"", extractLastCodeBlock(markdown))
    }

    @Test
    fun `fencedArtifactMarkdown uses longer outer fence for standalone inner fence`() {
        val content = "before\n```\nafter"
        val markdown = fencedArtifactMarkdown("markdown", content)

        assertEquals("````markdown\nbefore\n```\nafter\n````\n", markdown)
        assertEquals(content, extractLastCodeBlock(markdown))
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
            reviewTemplate,
            slashPromptTemplate("/review"),
        )
    }

    @Test
    fun `slashPromptTemplate appends suffix when present`() {
        val result = slashPromptTemplate("/review some specific code")
        assertEquals(
            "$reviewTemplate\n\nsome specific code",
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
            "$reviewTemplate\n\nmultiple words\nhere",
            result,
        )
    }

    @Test
    fun `slashPromptTemplate is case insensitive for commands`() {
        assertEquals(
            reviewTemplate,
            slashPromptTemplate("/REVIEW"),
        )
    }

    @Test
    fun `slashCommandPrefix returns command while typing`() {
        assertEquals("/login", slashCommandPrefix("/login"))
    }

    @Test
    fun `slashCommandPrefix stops after completed command inserts trailing space`() {
        assertNull(slashCommandPrefix("/login "))
    }

    @Test
    fun `isInternalHistoryUserMessage hides system reminder tails`() {
        assertTrue(
            isInternalHistoryUserMessage("<system-reminder>\nCurrent date: today\n</system-reminder>"),
        )
    }

    @Test
    fun `isInternalHistoryUserMessage hides verification nudges`() {
        assertTrue(
            isInternalHistoryUserMessage("You made code edits but have not verified them. Run a fast check (`cargo check`)."),
        )
    }
}
