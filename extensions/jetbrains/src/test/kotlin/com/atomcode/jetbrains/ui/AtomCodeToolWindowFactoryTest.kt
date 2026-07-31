package com.atomcode.jetbrains.ui

import com.intellij.icons.AllIcons
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertSame

class AtomCodeToolWindowFactoryTest {
    @Test
    fun `primary title action opens session history`() {
        assertEquals("Session History", PRIMARY_TITLE_ACTION_TEXT)
        assertEquals("Open AtomCode session history", PRIMARY_TITLE_ACTION_DESCRIPTION)
    }

    @Test
    fun `primary title action uses history icon`() {
        val action = createPrimaryTitleAction()

        assertSame(AllIcons.General.History, action.templatePresentation.icon)
    }

    @Test
    fun `title actions put new tab before session history and settings`() {
        assertEquals(
            listOf("New Tab", "Session History", "Settings"),
            createTitleActions().map { it.templatePresentation.text },
        )
    }
}
