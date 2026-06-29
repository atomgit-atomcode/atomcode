package com.atomcode.jetbrains.ui.header

import com.atomcode.jetbrains.daemon.ConnectionState
import com.intellij.ui.JBColor
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.FlowLayout
import javax.swing.BorderFactory
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.UIManager

/**
 * 极简 Header：仅状态指示器 + 应用名。
 * 会话管理和设置入口由 Tab 栏的 titleActions 提供。
 */
class HeaderPanel : JPanel(BorderLayout()) {
    private var initialized = false

    private val statusDot = JLabel("●").apply {
        foreground = DISCONNECTED_COLOR
        font = font.deriveFont(10f)
    }

    private val title = JLabel("AtomCode").apply {
        font = font.deriveFont(java.awt.Font.BOLD, font.size2D + 1f)
        // JBColor(亮色, 暗色)
        foreground = JBColor(0x1E1E1E, 0xE0E0E0)
    }

    init {
        isOpaque = true
        initialized = true
        applyTheme()

        val left = JPanel(FlowLayout(FlowLayout.LEFT, 6, 0)).apply {
            isOpaque = false
            add(statusDot)
            add(title)
        }
        add(left, BorderLayout.WEST)
    }

    override fun updateUI() {
        super.updateUI()
        if (initialized) applyTheme()
    }

    private fun applyTheme() {
        background = UIUtil.getPanelBackground()
        title.foreground = UIUtil.getLabelForeground()
        border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(
                0,
                0,
                1,
                0,
                UIManager.getColor("Component.borderColor") ?: JBColor.border(),
            ),
            BorderFactory.createEmptyBorder(5, 10, 5, 8),
        )
    }

    fun updateConnectionState(state: ConnectionState) {
        statusDot.foreground = when (state) {
            is ConnectionState.Ready -> CONNECTED_COLOR
            is ConnectionState.Error -> ERROR_COLOR
            ConnectionState.CheckingDaemon,
            ConnectionState.StartingDaemon,
            ConnectionState.Connecting,
            ConnectionState.SyncingProject,
            ConnectionState.CheckingProvider -> CONNECTING_COLOR
            else -> DISCONNECTED_COLOR
        }
        statusDot.toolTipText = when (state) {
            is ConnectionState.Ready -> "Connected (${state.daemonVersion})"
            is ConnectionState.Error -> state.message
            is ConnectionState.SetupRequired -> state.reason
            else -> "Not connected"
        }
    }

    companion object {
        // JBColor(亮色, 暗色)
        private val CONNECTED_COLOR = JBColor(0x2D8A6E, 0x4EC9B0)
        private val CONNECTING_COLOR = JBColor(0xAAAA00, 0xCCCC00)
        private val DISCONNECTED_COLOR = JBColor(0x999999, 0x888888)
        private val ERROR_COLOR = JBColor(0xC04040, 0xF44747)
    }
}
