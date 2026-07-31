package com.atomcode.jetbrains.ui

import java.util.Locale
import kotlin.test.Test
import kotlin.test.assertEquals

class GearMenuLabelsTest {
    @Test
    fun `gear menu labels are localized in Chinese`() {
        assertEquals(
            GearMenuLabels(
                connectStart = "🔌 连接 / 启动",
                provider = "服务商",
                createProvider = "创建服务商...",
                editProvider = "编辑服务商...",
                deleteProvider = "删除服务商...",
                thinkingSettings = "思考设置...",
                login = "🔑 登录",
                codingPlanSetup = "🚀 CodingPlan 配置",
                sessionHistory = "📋 历史会话...",
                renameSession = "✏️ 重命名会话",
                deleteSession = "🗑 删除会话",
                refreshSessions = "🔄 刷新会话",
                openChanges = "📂 打开变更",
                diagnostics = "🩺 诊断",
                settings = "⚙ 设置...",
            ),
            gearMenuLabels(Locale.SIMPLIFIED_CHINESE),
        )
    }

    @Test
    fun `gear menu labels remain English outside Chinese locales`() {
        assertEquals(
            GearMenuLabels(
                connectStart = "🔌 Connect / Start",
                provider = "Provider",
                createProvider = "Create Provider...",
                editProvider = "Edit Provider...",
                deleteProvider = "Delete Provider...",
                thinkingSettings = "Thinking Settings...",
                login = "🔑 Login",
                codingPlanSetup = "🚀 CodingPlan Setup",
                sessionHistory = "📋 Session History...",
                renameSession = "✏️ Rename Session",
                deleteSession = "🗑 Delete Session",
                refreshSessions = "🔄 Refresh Sessions",
                openChanges = "📂 Open Changes",
                diagnostics = "🩺 Diagnostics",
                settings = "⚙ Settings...",
            ),
            gearMenuLabels(Locale.US),
        )
    }
}
