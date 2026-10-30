package com.atomcode.jetbrains.ui

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

/**
 * 自动刷新换欢迎页的时机：欢迎页会清空消息区，所以"错误还挂着"的时候不能换，
 * 否则刚显示的错误会一闪而过（会话创建被拒时 currentSession 一直是 null）。
 */
class WelcomePagePolicyTest {
    @Test
    fun `a pending error keeps the message area instead of the welcome page`() {
        assertFalse(
            WelcomePagePolicy.shouldRenderAutomatically(
                hasSession = false,
                generating = false,
                lastError = "该会话正在终端或其他客户端中使用。",
            ),
        )
    }

    @Test
    fun `without a session and without an error the welcome page is shown`() {
        assertTrue(
            WelcomePagePolicy.shouldRenderAutomatically(
                hasSession = false,
                generating = false,
                lastError = null,
            ),
        )
    }

    @Test
    fun `a live session or a running turn never gets the welcome page`() {
        assertFalse(
            WelcomePagePolicy.shouldRenderAutomatically(
                hasSession = true,
                generating = false,
                lastError = null,
            ),
        )
        assertFalse(
            WelcomePagePolicy.shouldRenderAutomatically(
                hasSession = false,
                generating = true,
                lastError = null,
            ),
        )
    }

    @Test
    fun `a pending error keeps the live view instead of re-rendering history`() {
        // 失败的一轮不落历史：重渲染会按历史重画并清屏，错误就没了。
        assertEquals(
            false,
            WelcomePagePolicy.shouldRenderFinishedTurnFromHistory("该会话正在终端或其他客户端中使用。"),
        )
    }

    @Test
    fun `a clean turn is re-rendered from history`() {
        assertEquals(true, WelcomePagePolicy.shouldRenderFinishedTurnFromHistory(null))
    }
}
