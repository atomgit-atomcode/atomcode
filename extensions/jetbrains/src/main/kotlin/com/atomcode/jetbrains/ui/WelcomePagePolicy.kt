package com.atomcode.jetbrains.ui

/**
 * 自动刷新时要不要把消息区换成欢迎页。
 *
 * 欢迎页的渲染会 `clearMessages()` 整块清屏。会话还没建起来（例如创建被拒：
 * 会话正被终端或另一个 runtime 占用）时，`currentSession` 一直是 null，任何一次
 * 自动刷新都会把刚刚展示的错误抹掉——用户看到的就是"一闪而过"。
 *
 * 所以：错误还挂着就先不要换欢迎页，等下一次提问（会清掉错误）或会话真正建立
 * 起来再说。用户主动打开欢迎页（切换语言、删除会话）不受这条约束。
 */
internal object WelcomePagePolicy {
    fun shouldRenderAutomatically(
        hasSession: Boolean,
        generating: Boolean,
        lastError: String?,
    ): Boolean = !hasSession && !generating && lastError == null
}