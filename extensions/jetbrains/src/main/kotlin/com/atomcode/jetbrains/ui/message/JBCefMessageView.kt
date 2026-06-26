package com.atomcode.jetbrains.ui.message

import com.google.gson.Gson
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.editor.colors.EditorColorsManager
import com.intellij.ui.JBColor
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.Color
import javax.swing.BorderFactory
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.SwingUtilities
import javax.swing.UIManager

/**
 * 基于 JBCefBrowser (Chromium) 的聊天消息视图。
 *
 * 架构：
 * - Kotlin → JS: browser.cefBrowser.executeJavaScript()
 * - JS → Kotlin: JBCefJSQuery 注入回调，JS 扫描 window 获取
 * - JBCefJSQuery 通知页面脚本就绪，主 frame 的 load-end 事件作为兼容兜底
 */
class JBCefMessageView(
    private val onHomeAction: (String) -> Unit = {},
) : JPanel(BorderLayout()) {
    private val gson = Gson()
    private data class BrowserTheme(val dark: Boolean, val bg: String, val fg: String)
    private companion object {
        const val RENDERER_VERSION = "stream-markdown-v3"
    }

    private data class WelcomeContent(
        val language: String,
        val title: String,
        val subtitle: String,
        val quickStartTitle: String,
        val quickStart: List<String>,
        val actionsTitle: String,
        val actions: List<WelcomeAction>,
        val commandsTitle: String,
        val commands: List<WelcomeCommand>,
        val docsTitle: String,
        val docsText: String,
        val settings: String,
        val login: String,
        val showLogin: Boolean,
        val docs: String,
        val languageLabel: String,
    )
    private data class WelcomeAction(val name: String, val label: String)
    private data class WelcomeCommand(val command: String, val label: String, val action: String)

    // 延迟初始化：只在组件进入可显示层级后创建 JCEF 浏览器。
    private var bridge: JBCefMessageBridge? = null

    @Volatile private var jsReady = false
    private val pendingCalls = mutableListOf<String>()
    private var initialized = false
    private var assistantUpdateSequence = 0L

    /**
     * addNotify 在组件被添加到可见容器时调用。
     * 延迟创建 JBCefBrowser 避免在工具窗口还未显示时提前触发 JCEF 初始化。
     */
    override fun addNotify() {
        super.addNotify()
        if (!initialized) {
            initialized = true
            scheduleBrowserInit()
        }
    }

    private fun scheduleBrowserInit() {
        if (!isDisplayable) return
        ApplicationManager.getApplication().invokeLater({
            if (bridge == null && isDisplayable) {
                initBrowser()
            }
        }, ModalityState.nonModal())
    }

    private fun initBrowser() {
        val supported = try {
            JBCefMessageBridge.isSupported()
        } catch (_: LinkageError) {
            false
        } catch (_: ClassNotFoundException) {
            false
        }
        if (!supported) {
            showBrowserUnavailable()
            return
        }

        val newBridge = try {
            JBCefMessageBridge(
                { message ->
                    if (message.startsWith("home:")) {
                        SwingUtilities.invokeLater { onHomeAction(message.removePrefix("home:")) }
                    }
                },
                { markJsReady() },
            )
        } catch (_: LinkageError) {
            showBrowserUnavailable()
            return
        } catch (_: ReflectiveOperationException) {
            showBrowserUnavailable()
            return
        }
        bridge = newBridge
        add(newBridge.component, BorderLayout.CENTER)
        newBridge.loadHtml(buildChatHtml())
    }

    private fun showBrowserUnavailable() {
        removeAll()
        add(JLabel("AtomCode message rendering is unavailable in this IDE runtime.").apply {
            foreground = JBColor.GRAY
            border = BorderFactory.createEmptyBorder(16, 16, 16, 16)
        }, BorderLayout.NORTH)
        revalidate()
        repaint()
    }

    fun dispose() {
        pendingCalls.clear()
        bridge?.dispose()
        bridge = null
    }

    // ── Public API ──

    fun addUserMessage(
        text: String,
        contextSummary: List<String> = emptyList(),
        attachments: List<MessageAttachmentView> = contextSummary.map { MessageAttachmentView(displayName = it) },
    ) {
        sendRawJs("addUserMessage(${gson.toJson(text)},${gson.toJson(attachments)})")
    }
    fun beginAssistantTurn()                    { sendJs("beginAssistantTurn") }
    fun addAssistantMessage(text: String)       { sendJs("addAssistantMessage", text) }
    fun addCodeBlock(lang: String, code: String, file: String? = null) { sendJs("addCodeBlock", lang, code, file ?: "") }
    fun addToolCall(name: String, status: String, detail: String? = null, summary: String = "") {
        sendJs("addToolCall", name, status, detail ?: "", summary)
    }
    fun updateToolCall(name: String, status: String, detail: String? = null, summary: String = "") {
        sendJs("updateToolCall", name, status, detail ?: "", summary)
    }
    fun addError(text: String)                  { sendJs("addError", text) }
    fun addQueuedMessage(text: String)          { sendJs("addQueuedMessage", text) }
    fun addThinkingIndicator()                  { sendJs("addThinkingIndicator") }
    fun replaceThinkingWithAssistant(text: String) { sendJs("replaceThinkingWithAssistant", text) }
    fun removeThinkingIndicator()               { sendJs("removeThinkingIndicator") }
    fun addSystemMessage(text: String)          { sendJs("addSystemMessage", text) }
    fun addAssistantEvent(text: String)         { sendJs("addAssistantEvent", text) }
    fun addTurnSummary(label: String, rounds: Int, toolCalls: Int, duration: String, tokens: Int, failed: Boolean = false) {
        sendRawJs(
            "addTurnSummary(" +
                "${gson.toJson(label)}," +
                "$rounds," +
                "$toolCalls," +
                "${gson.toJson(duration)}," +
                "$tokens," +
                "$failed" +
                ")"
        )
    }
    fun addReasoningBlock(text: String)         { sendJs("addReasoningBlock", text) }
    fun updateReasoningBlock(text: String)      { sendJs("updateReasoningBlock", text) }
    fun updateLastAssistantMessage(text: String) {
        assistantUpdateSequence += 1
        sendJs("updateLastAssistantMessage", text, assistantUpdateSequence.toString())
    }
    fun appendAssistantDelta(text: String) {
        assistantUpdateSequence += 1
        sendJs("appendAssistantDelta", text, assistantUpdateSequence.toString())
    }
    fun updateArtifactCodeBlock(id: String, language: String, title: String?, content: String) {
        sendJs("updateArtifactCodeBlock", id, language, title ?: "", content)
    }
    fun finishAssistantMarkdownStream(text: String? = null) {
        if (text == null) {
            sendJs("finishAssistantMarkdownStream")
        } else {
            assistantUpdateSequence += 1
            sendJs("finishAssistantMarkdownStream", text, assistantUpdateSequence.toString())
        }
    }
    fun showStreamingCursor()                   { sendJs("showStreamingCursor") }
    fun hideStreamingCursor()                   { sendJs("hideStreamingCursor") }
    fun finishAssistantTurn()                   { sendJs("finishAssistantTurn") }
    fun clear()                                 { sendJs("clearMessages") }
    fun render(model: ChatRenderModel)          { sendRawJs("renderChatModel(${gson.toJson(model)})") }
    fun showWelcomePage(language: String = defaultWelcomeLanguage(), loggedIn: Boolean = false) {
        sendRawJs("showWelcomePage(${gson.toJson(welcomeContent(language, loggedIn))})")
    }

    // ── Internals ──

    private fun sendJs(fn: String, vararg args: String) {
        val escaped = args.joinToString(",") { arg -> gson.toJson(arg) }
        val call = "$fn($escaped)"
        if (jsReady) {
            executeJs(call)
        } else {
            pendingCalls.add(call)
        }
    }

    private fun sendRawJs(call: String) {
        if (jsReady) {
            executeJs(call)
        } else {
            pendingCalls.add(call)
        }
    }

    private fun markJsReady() {
        if (!SwingUtilities.isEventDispatchThread()) {
            SwingUtilities.invokeLater(::markJsReady)
            return
        }
        if (bridge == null) return
        if (jsReady) return
        jsReady = true
        executeJs("setTheme(${gson.toJson(currentBrowserTheme())})")
        flushPending()
    }

    override fun updateUI() {
        super.updateUI()
        if (initialized) {
            SwingUtilities.invokeLater {
                executeJs("setTheme(${gson.toJson(currentBrowserTheme())})")
            }
        }
    }

    private fun flushPending() {
        pendingCalls.forEach { executeJs(it) }
        pendingCalls.clear()
    }

    private fun executeJs(code: String) {
        bridge?.executeJavaScript(code)
    }

    private fun isDarkTheme(background: Color): Boolean {
        val luminance = (0.2126 * background.red + 0.7152 * background.green + 0.0722 * background.blue) / 255.0
        return luminance < 0.5
    }

    private fun currentBrowserTheme(): BrowserTheme {
        val scheme = EditorColorsManager.getInstance().globalScheme
        val bg = scheme.defaultBackground
            ?: UIManager.getColor("EditorPane.background")
            ?: UIUtil.getPanelBackground()
        val fg = scheme.defaultForeground
            ?: UIManager.getColor("EditorPane.foreground")
            ?: UIUtil.getLabelForeground()
        return BrowserTheme(isDarkTheme(bg), bg.toCss(), fg.toCss())
    }

    private fun Color.toCss(): String = "#%02x%02x%02x".format(red, green, blue)

    private fun defaultWelcomeLanguage(): String =
        if (java.util.Locale.getDefault().language.equals("zh", ignoreCase = true)) "zh" else "en"

    private fun welcomeContent(language: String, loggedIn: Boolean): WelcomeContent =
        if (language == "zh") {
            WelcomeContent(
                language = "zh",
                title = "AtomCode",
                subtitle = "智能编码助手",
                quickStartTitle = "快速开始",
                quickStart = listOf(
                    "直接在下方输入你的任务，按 Enter 发送。",
                    "选中代码后，用右键菜单或 Alt+Enter 调用 AtomCode。",
                    "用 Add Selection/File as Context 附加文件或选区。",
                ),
                actionsTitle = "选中代码后",
                actions = listOf(
                    WelcomeAction("Explain Selection", "解释代码意图和实现方式"),
                    WelcomeAction("Fix Selection", "修复选中片段中的问题"),
                    WelcomeAction("Optimize Selection", "优化性能和可读性"),
                    WelcomeAction("Add Selection/File as Context", "把文件或选区附加到下一条消息"),
                ),
                commandsTitle = "输入框命令",
                commands = listOf(
                    WelcomeCommand("/review", "填入代码审查提示，可继续补充范围或要求", "review"),
                ),
                docsTitle = "连接与帮助",
                docsText = "还没配置模型时，先打开设置或登录 AtomGit；遇到问题可查看文档。",
                settings = "AtomCode 设置",
                login = "登录 AtomGit",
                showLogin = !loggedIn,
                docs = "查看文档",
                languageLabel = "语言",
            )
        } else {
            WelcomeContent(
                language = "en",
                title = "AtomCode",
                subtitle = "AI coding assistant",
                quickStartTitle = "Quick Start",
                quickStart = listOf(
                    "Type a task below and press Enter to send.",
                    "Select code, then use the popup menu or Alt+Enter.",
                    "Use Add Selection/File as Context to attach code first.",
                ),
                actionsTitle = "With Selected Code",
                actions = listOf(
                    WelcomeAction("Explain Selection", "Explain intent and implementation"),
                    WelcomeAction("Fix Selection", "Repair issues in the selected range"),
                    WelcomeAction("Optimize Selection", "Improve performance and readability"),
                    WelcomeAction("Add Selection/File as Context", "Attach a file or range to the next message"),
                ),
                commandsTitle = "Input Commands",
                commands = listOf(
                    WelcomeCommand("/review", "Insert a review prompt, then add scope or constraints", "review"),
                ),
                docsTitle = "Connect & Help",
                docsText = "If no model is configured yet, open settings or sign in to AtomGit. For troubleshooting, open the docs.",
                settings = "AtomCode Menu",
                login = "Sign in",
                showLogin = !loggedIn,
                docs = "Open Docs",
                languageLabel = "Language",
            )
        }

    // ── Inline HTML (避免 classpath 资源加载问题) ──

    private fun buildChatHtml(): String {
        val markedScript = loadWebScript("/markdown/marked.min.js")
        val purifyScript = loadWebScript("/markdown/purify.min.js")
        val theme = currentBrowserTheme()
        val dark = theme.dark
        val bg = theme.bg
        val fg = theme.fg
        val ubg = if (dark) "#094771" else "#d0e4f7"
        val ufg = if (dark) "#e0e0e0" else "#1e1e1e"
        val afg = if (dark) "#d4d4d4" else "#333"
        val cbg = if (dark) "#1e1e1e" else "#fafafa" // code
        val cbo = if (dark) "#3c3c3c" else "#ccc"    // code border
        val chb = if (dark) "#2d2d2d" else "#e0e0e0" // code head bg
        val chf = if (dark) "#9cdcfe" else "#005a9e" // code head fg
        val tbg = if (dark) "#252526" else "#f4f4f4" // tool
        val tbo = if (dark) "#3c3c3c" else "#d8d8d8"
        val tfg = if (dark) "#a7a7a7" else "#666"
        val ebg = if (dark) "#3d2020" else "#f8e0e0" // error
        val ebo = if (dark) "#5a3030" else "#d8a0a0"
        val efg = if (dark) "#f48771" else "#c04040"
        val qbg = if (dark) "#1a3550" else "#e8eef4" // queued
        val qfg = if (dark) "#8899aa" else "#667788"
        val rbg = if (dark) "#1a2330" else "#f0f4f8" // reason
        val rbo = if (dark) "#2a3a4a" else "#d0d8e0"
        val sfg = if (dark) "#888" else "#666"       // system
        val vfg = if (dark) "#8fbc72" else "#4f7f3a" // avatar

        return """
<!DOCTYPE html><html><head><meta charset="UTF-8"><style>
*{margin:0;padding:0;box-sizing:border-box}
	body{background:$bg;color:$fg;font:13px -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;line-height:1.55;padding:18px 20px 28px;overflow-y:auto}
	#m{display:flex;flex-direction:column;gap:18px;width:100%;max-width:920px;margin:0 auto}
	.um{display:flex;justify-content:flex-end;padding-left:48px}
	.um .u-card{background:$ubg;color:$ufg;border:1px solid ${if (dark) "#245b82" else "#b8d3e7"};border-radius:14px 14px 4px 14px;max-width:82%;min-width:180px;overflow:hidden;box-shadow:0 1px 2px rgba(0,0,0,.12)}
	.um .u-text{padding:9px 13px;white-space:pre-wrap;word-break:break-word}
	.um .u-text:empty{display:none}
	.um .u-files{display:flex;flex-direction:column;gap:1px;padding:6px;border-top:1px solid ${if (dark) "rgba(255,255,255,.12)" else "rgba(0,70,115,.14)"};background:${if (dark) "rgba(0,0,0,.10)" else "rgba(255,255,255,.28)"}}
	.um .u-file{display:grid;grid-template-columns:28px minmax(0,1fr);gap:8px;align-items:center;padding:6px 7px;border-radius:7px;background:${if (dark) "rgba(255,255,255,.055)" else "rgba(255,255,255,.55)"}}
	.um .u-file-icon{display:flex;align-items:center;justify-content:center;width:28px;height:28px;border-radius:6px;background:${if (dark) "#21405a" else "#e4f1fa"};color:${if (dark) "#9bd3f5" else "#286c99"};font:700 8px 'JetBrains Mono','Consolas',monospace;text-transform:uppercase}
	.um .u-file-copy{min-width:0;line-height:1.25}
	.um .u-file-name{font-size:11px;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
	.um .u-file-path{margin-top:2px;color:${if (dark) "#a9c3d5" else "#55768c"};font-size:9px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
	.um .u-image{display:block;width:100%;padding:0;border:0;border-radius:8px;overflow:hidden;background:${if (dark) "rgba(255,255,255,.055)" else "rgba(255,255,255,.55)"};color:inherit;text-align:left;cursor:pointer}
	.um .u-image img{display:block;max-width:220px;max-height:150px;width:auto;height:auto;object-fit:contain;background:${if (dark) "rgba(0,0,0,.18)" else "rgba(255,255,255,.35)"}}
	.um .u-image-name{display:block;padding:5px 7px;font-size:10px;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
	.img-modal{position:fixed;inset:0;z-index:20;display:flex;align-items:center;justify-content:center;padding:28px;background:rgba(0,0,0,.68)}
	.img-modal button{position:absolute;inset:0;border:0;background:transparent;cursor:zoom-out}
	.img-modal figure{position:relative;z-index:1;max-width:94vw;max-height:92vh;margin:0;padding:10px;border-radius:10px;background:${if (dark) "#202326" else "#f8fbfd"};box-shadow:0 18px 50px rgba(0,0,0,.35)}
	.img-modal img{display:block;max-width:calc(94vw - 20px);max-height:calc(92vh - 46px);object-fit:contain}
	.img-modal figcaption{margin-top:7px;color:${if (dark) "#cfd4da" else "#3d4650"};font-size:11px;text-align:center;max-width:calc(94vw - 20px);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
	.home{display:flex;flex-direction:column;gap:10px;width:100%;max-width:1160px;margin:4px auto 0;padding:0 0 12px}
	.home-hero{border:1px solid $cbo;border-radius:10px;background:${if (dark) "#252526" else "#f8fafc"};padding:12px 14px;box-shadow:0 1px 2px rgba(0,0,0,.08)}
	.home-head{display:flex;align-items:center;justify-content:space-between;gap:12px;min-width:0}
	.home-brand{display:flex;align-items:center;gap:9px;min-width:0}
	.home-title{display:flex;align-items:center;gap:8px;font-size:22px;font-weight:750;line-height:1.15;color:$fg;min-width:0}
	.home-mark{display:inline-flex;align-items:center;justify-content:center;width:30px;height:30px;border-radius:7px;background:${if (dark) "#293424" else "#edf5e9"};color:$vfg;font-size:15px;font-weight:800}
	.home-subtitle{margin-top:2px;color:$sfg;font-size:12px;white-space:nowrap}
	.home-actions{display:flex;flex-wrap:wrap;gap:8px;margin-top:10px;max-width:520px}
	.home-btn{border:1px solid $cbo;border-radius:7px;background:${if (dark) "#2d2d2d" else "#ffffff"};color:$fg;padding:6px 10px;min-width:82px;font:12px -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;cursor:pointer}
	.home-btn.primary{background:$ubg;border-color:${if (dark) "#245b82" else "#b8d3e7"};color:$ufg}
	.home-btn:hover{filter:brightness(${if (dark) "1.14" else ".97"})}
	.home-lang{display:inline-flex;align-items:center;gap:0;flex:0 0 auto;border:1px solid $cbo;border-radius:8px;overflow:hidden;background:${if (dark) "#2d2d2d" else "#ffffff"}}
	.home-lang-label{padding:5px 8px;color:$sfg;font-size:11px;border-right:1px solid $cbo}
	.home-lang button{min-width:58px;border:0;border-radius:0;background:transparent;padding:5px 9px}
	.home-lang button+button{border-left:1px solid $cbo}
	.home-grid{display:grid;grid-template-columns:1fr;gap:10px;align-items:stretch}
	.home-section{border:1px solid $cbo;border-radius:8px;padding:10px 12px;background:${if (dark) "rgba(255,255,255,.025)" else "#ffffff"};min-width:0;height:100%}
	.home-section h2{margin:0 0 7px;font-size:13px;line-height:1.2;color:$fg}
	.home-section ul{margin:0;padding-left:17px;color:$afg}
	.home-section li{margin:3px 0}
	.command-list{display:flex;flex-direction:column;gap:7px}
	.action-list{display:grid;grid-template-columns:1fr;gap:7px 10px}
	.action-row{display:flex;flex-direction:column;gap:2px;min-width:0}
	.action-row strong{font-size:12px;color:$fg}
	.action-row span{min-width:0;color:$afg;font-size:12px}
	.command-row{display:grid;grid-template-columns:minmax(78px,max-content) minmax(0,1fr);gap:10px;align-items:center}
	.command-row button{border:1px solid $cbo;border-radius:6px;background:$cbg;color:$chf;padding:5px 7px;text-align:left;font:12px 'JetBrains Mono','Consolas',monospace;cursor:pointer}
	.command-row code{border:1px solid $cbo;border-radius:6px;background:$cbg;color:$chf;padding:5px 7px;font:12px 'JetBrains Mono','Consolas',monospace}
	.command-row span{min-width:0;color:$afg;font-size:12px}
	.home-doc{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:10px;align-items:center}
	.home-doc p{margin:0;color:$afg;font-size:12px;min-width:0}
	@media(min-width:720px){.home-grid{grid-template-columns:1fr 1fr}.action-list{grid-template-columns:1fr 1fr}}
	@media(max-width:640px){.home-head{flex-direction:column;align-items:flex-start}.home-actions{align-items:stretch}.home-btn{flex:1}.home-lang{align-self:flex-start}.home-doc{align-items:flex-start;grid-template-columns:1fr}}
	.am{display:flex;flex-direction:column;align-items:stretch;min-width:0}
	.am .av{display:flex;align-items:center;gap:7px;color:$vfg;font-size:11px;font-weight:600;letter-spacing:.01em;margin-bottom:6px}
	.am .av:before{content:'A';display:inline-flex;align-items:center;justify-content:center;width:18px;height:18px;border-radius:5px;background:${if (dark) "#293424" else "#edf5e9"};color:$vfg;font-size:10px;font-weight:700}
	.am .parts{display:flex;flex-direction:column;gap:6px;align-items:stretch;width:100%;padding-left:25px}
	.am .b{color:$afg;padding:0;max-width:100%;white-space:normal;word-break:break-word}
	.am .b:empty{display:none}
	.am .b h1,.am .b h2,.am .b h3,.am .b h4{margin:10px 0 6px;line-height:1.28}
	.am .b h1{font-size:1.22em}.am .b h2{font-size:1.14em}.am .b h3{font-size:1.06em}.am .b h4{font-size:1em}
	.am .b p{margin:4px 0}
	.am .b ul,.am .b ol{margin:5px 0;padding-left:22px}
	.am .b li{margin:2px 0}
	.am .b li>p{margin:2px 0}
	.am .b blockquote{margin:8px 0;padding-left:10px;border-left:3px solid $cbo;color:$sfg}
	.am .b code{font:12px 'JetBrains Mono','Consolas',monospace;background:$cbg;border-radius:3px;padding:1px 4px;word-break:normal}
	.am .b pre{margin:7px 0;padding:9px 11px;overflow:auto;white-space:pre;background:$cbg;border:1px solid $cbo;border-radius:6px;max-width:100%}
	.am .b pre code{display:block;padding:0;background:transparent;white-space:pre;word-break:normal}
	.am .b table{border-collapse:collapse;margin:8px 0;max-width:100%;display:block;overflow-x:auto}
	.am .b th,.am .b td{border:1px solid $cbo;padding:5px 8px;text-align:left}
	.am .b a{color:$chf}
	.am .b>:first-child{margin-top:0}.am .b>:last-child{margin-bottom:0}
.cm{border:1px solid $cbo;border-radius:8px;overflow:hidden;background:$cbg;margin:8px 0 10px;box-shadow:0 1px 2px rgba(0,0,0,.06)}
.cm .h{display:flex;align-items:center;min-height:30px;background:$chb;color:$sfg;padding:0 10px;border-bottom:1px solid $cbo}
.cm .lang{display:inline-flex;align-items:center;height:20px;border:1px solid $cbo;border-radius:4px;background:${if (dark) "rgba(255,255,255,.045)" else "rgba(255,255,255,.82)"};color:$chf;padding:0 7px;font:600 11px 'JetBrains Mono','Consolas',monospace;line-height:20px}
.cm pre{margin:0;padding:13px 15px;font:12px 'JetBrains Mono','Consolas',monospace;line-height:1.62;overflow-x:auto;white-space:pre;color:$fg;background:$cbg;border:0;border-radius:0;max-width:100%}
.cm-diff pre{padding:0;line-height:1.5;background:${if (dark) "#15191f" else "#ffffff"}}
.dl{display:block;min-width:max-content;padding:7px 0}
.dr{display:grid;grid-template-columns:30px minmax(0,1fr);min-height:22px}
.dr .sg{display:flex;align-items:center;justify-content:center;user-select:none;color:$sfg;border-right:1px solid ${if (dark) "rgba(255,255,255,.07)" else "rgba(31,35,40,.08)"}}
.dr .tx{display:block;padding:1px 14px 1px 12px;white-space:pre}
.dr.add{background:${if (dark) "rgba(46,160,67,.18)" else "#dafbe1"};color:${if (dark) "#c9f0d0" else "#116329"}}
.dr.del{background:${if (dark) "rgba(248,81,73,.18)" else "#ffebe9"};color:${if (dark) "#ffd1d1" else "#82071e"}}
.dr.hunk{background:${if (dark) "rgba(56,139,253,.16)" else "#ddf4ff"};color:${if (dark) "#a5d6ff" else "#0969da"}}
.dr.meta{background:${if (dark) "rgba(139,148,158,.13)" else "#f6f8fa"};color:$sfg}
.dr.ctx{color:$fg}
.dr.add .sg{color:${if (dark) "#7ee787" else "#1a7f37"}}
.dr.del .sg{color:${if (dark) "#ffa198" else "#cf222e"}}
.dr.hunk .sg,.dr.meta .sg{color:inherit}
		.tm{color:$tfg;font-size:12px;min-width:0}
	.tm details{border-radius:6px}
	.tm details[open]{background:$tbg;border:1px solid $tbo}
	.tm summary{display:flex;align-items:center;gap:7px;min-height:28px;padding:4px 8px;cursor:pointer;list-style:none;border-radius:6px;white-space:nowrap;overflow:hidden}
	.tm summary:hover{background:$tbg;color:$fg}
	.tm summary::-webkit-details-marker{display:none}
	.tm .chev{width:10px;color:$sfg;font-size:10px;transition:transform .12s ease}
	.tm details[open] .chev{transform:rotate(90deg)}
	.tm .tool-dot{width:6px;height:6px;border-radius:50%;background:$sfg;flex:0 0 auto}
	.tm.ts-success .tool-dot{background:${if (dark) "#73a857" else "#5c8f43"}}
	.tm.ts-running .tool-dot{background:$chf;box-shadow:0 0 0 3px ${if (dark) "rgba(156,220,254,.12)" else "rgba(0,90,158,.10)"}}
	.tm.ts-error .tool-dot{background:$efg}
	.tm .tool-name{color:$fg;font-family:'JetBrains Mono','Consolas',monospace;overflow:hidden;text-overflow:ellipsis}
	.tm .tool-summary{min-width:0;flex:1;margin-left:5px;color:$sfg;font:11px 'JetBrains Mono','Consolas',monospace;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
	.tm .tool-status{margin-left:8px;color:$sfg;font-size:11px;overflow:hidden;text-overflow:ellipsis;flex:0 0 auto}
	.tm pre{margin:0;border-top:1px solid $tbo;max-height:280px;overflow:auto;white-space:pre-wrap;word-break:break-word;color:$fg;background:$cbg;padding:9px 12px;font:11px/1.5 'JetBrains Mono','Consolas',monospace}
.em{border:1px solid $ebo;border-radius:6px;background:$ebg;color:$efg;padding:6px 10px;font-size:12px}
.qm{display:flex;justify-content:flex-end}
.qm .b{background:$qbg;color:$qfg;padding:6px 12px;border-radius:10px 4px 10px 10px;max-width:78%;font-size:11px}
.rm{border-left:2px solid $rbo;background:$rbg;color:$sfg;padding:5px 9px;font-size:11px;max-width:100%;margin-bottom:2px}
.sm{color:$sfg;font-size:11px;padding:1px 8px}
.turn-summary{display:flex;align-items:center;gap:9px;color:$sfg;font:11px 'JetBrains Mono','Consolas',monospace;margin:8px 0 4px;opacity:.82;white-space:nowrap}
.turn-summary:before,.turn-summary:after{content:'';height:1px;background:$cbo;flex:1;min-width:24px;opacity:.95}
.turn-summary.failed{color:$efg}.turn-summary.failed:before,.turn-summary.failed:after{background:$ebo}
.th{display:flex;flex-direction:column;align-items:flex-start;color:$sfg}
.th .av{color:$vfg;font-size:11px;margin-bottom:2px}
	.dots::after{content:'';animation:d 1.5s steps(4,end) infinite}
	.streaming-cursor{display:inline-block;width:7px;height:1.1em;margin-left:2px;background:$afg;vertical-align:-2px;animation:blink 1s steps(2,start) infinite}
	@keyframes d{0%{content:''}25%{content:'.'}50%{content:'..'}75%{content:'...'}}
	@keyframes blink{0%,45%{opacity:1}46%,100%{opacity:0}}
</style></head><body data-renderer="$RENDERER_VERSION">
<div id="m"></div>
<script>$markedScript</script>
<script>$purifyScript</script>
<script>
		var m=document.getElementById('m'),last=null,active=null,ti=-1,nb=true,cv=false,sr=0,useq=0,ss=null;
	function scroller(){return document.scrollingElement||document.documentElement||document.body}
	function updateNearBottom(){var e=scroller();nb=e.scrollHeight-e.scrollTop-e.clientHeight<120}
	document.addEventListener('scroll',updateNearBottom,true);
	function sd(force){
		if(force)nb=true;
		if(!nb)return;
		if(sr)cancelAnimationFrame(sr);
		sr=requestAnimationFrame(function(){var e=scroller();e.scrollTop=e.scrollHeight;sr=0})
	}
	if(typeof ResizeObserver!=='undefined')new ResizeObserver(function(){sd()}).observe(m);
function h(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;')}
	function host(action){if(typeof window.atomcodeHost==='function'){window.atomcodeHost('home:'+action);return}for(var k in window){if(k.indexOf('JBCefQuery_')===0&&typeof window[k]==='function'){window[k]('home:'+action);return}}}
	function clearHome(){var x=m.querySelector('.home');if(x)x.remove()}
	function switchWelcomeLanguage(lang){host('language:'+lang)}
	function md(s){
		var source=String(s||'');
		if(typeof marked==='undefined'||typeof DOMPurify==='undefined')return h(source).replace(/\n/g,'<br>');
		try{return DOMPurify.sanitize(marked.parse(source,{gfm:true,breaks:true,renderer:markdownRenderer()}),{USE_PROFILES:{html:true}})}
		catch(e){return h(source).replace(/\n/g,'<br>')}
	}
	function codeBlockHtml(lang,text){
		var block=normalizeCodeBlock(lang,text);
		if(isDiffLang(block.lang))return diffBlockHtml(block.lang,block.text);
		return '<div class="cm"><div class="h"><span class="lang">'+h(normalizeCodeLang(block.lang))+'</span></div><pre>'+h(block.text)+'</pre></div>'
	}
	function diffBlockHtml(lang,text){
		return '<div class="cm cm-diff"><div class="h"><span class="lang">'+h(normalizeCodeLang(lang))+'</span></div><pre><span class="dl">'+diffLinesHtml(text)+'</span></pre></div>'
	}
	function diffLinesHtml(text){
		var lines=String(text||'').replace(/\r\n/g,'\n').replace(/\r/g,'\n').split('\n');
		if(lines.length&&lines[lines.length-1]==='')lines.pop();
		if(!lines.length)return '<span class="dr ctx"><span class="sg"></span><span class="tx"></span></span>';
		return lines.map(function(line){
			var kind='ctx',sign=' ';
			if(line.indexOf('@@')===0){kind='hunk';sign='@'}
			else if(line.indexOf('diff --git')===0||line.indexOf('index ')===0||line.indexOf('--- ')===0||line.indexOf('+++ ')===0){kind='meta';sign='·'}
			else if(line.charAt(0)==='+'){kind='add';sign='+'}
			else if(line.charAt(0)==='-'){kind='del';sign='-'}
			return '<span class="dr '+kind+'"><span class="sg">'+h(sign)+'</span><span class="tx">'+h(line)+'</span></span>'
		}).join('')
	}
	function normalizeCodeBlock(lang,text){
		var code=String(text||''),raw=String(lang||''),base=rawCodeLang(raw);
		if((!base||base==='code'||base==='text'||base==='plain')&&looksLikeLanguagePrefix(code,'diff'))return {lang:'diff',text:dropFirstLine(code)};
		return {lang:raw,text:code}
	}
	function looksLikeLanguagePrefix(text,name){
		var line=String(text||'').split(/\r?\n/,1)[0].replace(/^\s+|\s+$/g,'').toLowerCase();
		return line===name||line===name+':'
	}
	function dropFirstLine(text){
		text=String(text||'');
		var n=text.indexOf('\n');
		return n>=0?text.substring(n+1):''
	}
	function rawCodeLang(lang){
		lang=String(lang||'').replace(/^\s+|\s+$/g,'');
		if(!lang)return '';
		return lang.split(/\s+/)[0].toLowerCase()
	}
	function isDiffLang(lang){
		var l=rawCodeLang(lang);
		return l==='diff'||l==='patch'
	}
	function markdownRenderer(){
		if(typeof marked.Renderer!=='function')return undefined;
		var r=new marked.Renderer();
		r.code=function(token,infostring){
			var text='',lang='';
			if(token&&typeof token==='object'){text=token.text||'';lang=token.lang||''}
			else{text=String(token||'');lang=String(infostring||'')}
			return codeBlockHtml(lang,text)
		};
		return r
	}
	function normalizeCodeLang(lang){
		lang=String(lang||'').replace(/^\s+|\s+$/g,'');
		if(!lang)return 'Code';
		var token=lang.split(/\s+/)[0];
		if(rawCodeLang(token)==='diff')return 'Diff';
		if(rawCodeLang(token)==='patch')return 'Patch';
		return token.replace(/[-_]+/g,' ')||'Code'
	}
	function codeLang(code){
		var cls=String(code.className||''),m=cls.match(/(?:^|\s)language-([^\s]+)/);
		if(!m)return 'Code';
		return normalizeCodeLang(m[1])
	}
	function enhanceMarkdown(root){
		var blocks=root.querySelectorAll('pre>code');
		Array.prototype.forEach.call(blocks,function(code){
			var pre=code.parentNode;
			if(!pre||!pre.parentNode||(pre.parentNode.classList&&pre.parentNode.classList.contains('cm')))return;
			var block=normalizeCodeBlock(codeLang(code),code.textContent||'');
			if(isDiffLang(block.lang)){
				var wrapper=document.createElement('div');
				wrapper.innerHTML=diffBlockHtml(block.lang,block.text);
				pre.parentNode.insertBefore(wrapper.firstChild,pre);
				pre.remove();
				return
			}
			var box=document.createElement('div');
			box.className='cm';
			var head=document.createElement('div');
			head.className='h';
			head.innerHTML='<span class="lang">'+h(codeLang(code))+'</span>';
			pre.parentNode.insertBefore(box,pre);
			box.appendChild(head);
			box.appendChild(pre);
		})
	}
	function stripLineBreak(line){
		line=String(line||'');
		if(line.charAt(line.length-1)==='\n')line=line.substring(0,line.length-1);
		if(line.charAt(line.length-1)==='\r')line=line.substring(0,line.length-1);
		return line
	}
	function fenceOpen(line){
		var raw=stripLineBreak(line),i=0,indent=0;
		while(i<raw.length&&(raw.charAt(i)===' '||raw.charAt(i)==='\t')){indent++;i++}
		if(indent>3)return null;
		var ch=raw.charAt(i);
		if(ch!=='`'&&ch!=='~')return null;
		var j=i;
		while(j<raw.length&&raw.charAt(j)===ch)j++;
		if(j-i<3)return null;
		var info=raw.substring(j).trim();
		if(ch==='`'&&info.indexOf('`')>=0)return null;
		return {ch:ch,len:j-i,lang:normalizeCodeLang(info)}
	}
	function fenceClose(line,state){
		var raw=stripLineBreak(line),i=0,indent=0;
		while(i<raw.length&&(raw.charAt(i)===' '||raw.charAt(i)==='\t')){indent++;i++}
		if(indent>3)return false;
		var j=i;
		while(j<raw.length&&raw.charAt(j)===state.ch)j++;
		return j-i>=state.len&&raw.substring(j).trim()===''
	}
	function newStreamState(target){
		return {target:target,raw:''}
	}
	function currentStreamState(){
		var b=textSegment();
		if(!ss||ss.target!==b)ss=newStreamState(b);
		return ss
	}
	function repairStreamingMarkdown(source){
		source=String(source||'');
		var lines=source.split('\n'),open=null;
		for(var i=0;i<lines.length;i++){
			if(!open){
				open=fenceOpen(lines[i])
			}else if(fenceClose(lines[i],open)){
				open=null
			}
		}
		if(!open)return source;
		return source+(source.charAt(source.length-1)==='\n'?'':'\n')+new Array(open.len+1).join(open.ch)+'\n'
	}
	function renderStreamState(state,final){
		var source=final?state.raw:repairStreamingMarkdown(state.raw);
		state.target.innerHTML=md(source);
		enhanceMarkdown(state.target)
	}
	function writeStreamText(state,text){
		state.raw+=String(text||'');
		renderStreamState(state,false)
	}
	function tv(d){return d?{
		bg:'#1e1e1e',fg:'#d4d4d4',ubg:'#094771',ufg:'#e0e0e0',ub:'#245b82',afg:'#d4d4d4',
			cbg:'#1b1f24',cbo:'#3b444f',chb:'#222831',chf:'#9cdcfe',tbg:'#252526',tbo:'#3c3c3c',
		tfg:'#a7a7a7',ebg:'#3d2020',ebo:'#5a3030',efg:'#f48771',qbg:'#1a3550',qfg:'#8899aa',
		rbg:'#1a2330',rbo:'#2a3a4a',sfg:'#888',vfg:'#8fbc72',avbg:'#293424',fileBg:'rgba(255,255,255,.055)',
		fileIconBg:'#21405a',fileIconFg:'#9bd3f5'
	}:{
		bg:'#ffffff',fg:'#1e1e1e',ubg:'#d0e4f7',ufg:'#1e1e1e',ub:'#b8d3e7',afg:'#333333',
			cbg:'#f8fafc',cbo:'#d6dbe1',chb:'#f2f5f8',chf:'#286c99',tbg:'#f4f4f4',tbo:'#d8d8d8',
		tfg:'#666666',ebg:'#f8e0e0',ebo:'#d8a0a0',efg:'#c04040',qbg:'#e8eef4',qfg:'#667788',
		rbg:'#f0f4f8',rbo:'#d0d8e0',sfg:'#666666',vfg:'#4f7f3a',avbg:'#edf5e9',fileBg:'rgba(255,255,255,.55)',
		fileIconBg:'#e4f1fa',fileIconFg:'#286c99'
	}}
	function setTheme(theme){
		var d=typeof theme==='object'?!!theme.dark:!!theme,v=tv(d),s=document.getElementById('theme-override');
		if(theme&&typeof theme==='object'){v.bg=theme.bg||v.bg;v.fg=theme.fg||v.fg}
		if(!s){s=document.createElement('style');s.id='theme-override';document.head.appendChild(s)}
		s.textContent='body{background:'+v.bg+'!important;color:'+v.fg+'!important}'+
		'.um .u-card{background:'+v.ubg+'!important;color:'+v.ufg+'!important;border-color:'+v.ub+'!important}'+
		'.um .u-file{background:'+v.fileBg+'!important}.um .u-file-icon{background:'+v.fileIconBg+'!important;color:'+v.fileIconFg+'!important}'+
		'.am .av{color:'+v.vfg+'!important}.am .av:before{background:'+v.avbg+'!important;color:'+v.vfg+'!important}'+
		'.am .b{color:'+v.afg+'!important}.am .b blockquote{border-left-color:'+v.cbo+'!important;color:'+v.sfg+'!important}'+
		'.am .b code,.am .b pre,.cm{background:'+v.cbg+'!important;border-color:'+v.cbo+'!important}.am .b th,.am .b td{border-color:'+v.cbo+'!important}.am .b a{color:'+v.chf+'!important}'+
			'.cm .h{background:'+v.chb+'!important;border-bottom-color:'+v.cbo+'!important}.cm .lang{border-color:'+v.cbo+'!important;color:'+v.chf+'!important}.cm pre{background:'+v.cbg+'!important;color:'+v.fg+'!important;border:0!important}'+
			'.cm-diff pre{background:'+(d?'#15191f':'#ffffff')+'!important}.dr .sg{border-right-color:'+(d?'rgba(255,255,255,.07)':'rgba(31,35,40,.08)')+'!important}.dr.add{background:'+(d?'rgba(46,160,67,.18)':'#dafbe1')+'!important;color:'+(d?'#c9f0d0':'#116329')+'!important}.dr.del{background:'+(d?'rgba(248,81,73,.18)':'#ffebe9')+'!important;color:'+(d?'#ffd1d1':'#82071e')+'!important}.dr.hunk{background:'+(d?'rgba(56,139,253,.16)':'#ddf4ff')+'!important;color:'+(d?'#a5d6ff':'#0969da')+'!important}.dr.meta{background:'+(d?'rgba(139,148,158,.13)':'#f6f8fa')+'!important;color:'+v.sfg+'!important}.dr.add .sg{color:'+(d?'#7ee787':'#1a7f37')+'!important}.dr.del .sg{color:'+(d?'#ffa198':'#cf222e')+'!important}'+
			'.tm{color:'+v.tfg+'!important}.tm details[open],.tm summary:hover{background:'+v.tbg+'!important;border-color:'+v.tbo+'!important;color:'+v.fg+'!important}'+
		'.tm .chev,.tm .tool-dot,.tm .tool-summary,.tm .tool-status{color:'+v.sfg+'!important}.tm .tool-name,.tm pre{color:'+v.fg+'!important}.tm pre{background:'+v.cbg+'!important;border-top-color:'+v.tbo+'!important}'+
		'.em{background:'+v.ebg+'!important;border-color:'+v.ebo+'!important;color:'+v.efg+'!important}.qm .b{background:'+v.qbg+'!important;color:'+v.qfg+'!important}'+
		'.rm{background:'+v.rbg+'!important;border-left-color:'+v.rbo+'!important;color:'+v.sfg+'!important}.sm,.th,.turn-summary{color:'+v.sfg+'!important}.turn-summary:before,.turn-summary:after{background:'+v.cbo+'!important}.turn-summary.failed{color:'+v.efg+'!important}.turn-summary.failed:before,.turn-summary.failed:after{background:'+v.ebo+'!important}.streaming-cursor{background:'+v.afg+'!important}'+
		'.home-hero,.home-section{background:'+v.tbg+'!important;border-color:'+v.cbo+'!important;color:'+v.fg+'!important}'+
		'.home-title,.home-section h2,.action-row strong{color:'+v.fg+'!important}.home-subtitle,.home-section ul,.action-row span,.command-row span,.home-doc p{color:'+v.afg+'!important}'+
		'.home-mark{background:'+v.avbg+'!important;color:'+v.vfg+'!important}.home-btn{background:'+v.cbg+'!important;border-color:'+v.cbo+'!important;color:'+v.fg+'!important}.home-btn.primary{background:'+v.ubg+'!important;border-color:'+v.ub+'!important;color:'+v.ufg+'!important}'+
		'.home-lang{background:'+v.cbg+'!important;border-color:'+v.cbo+'!important}.home-lang-label,.home-lang button+button{border-color:'+v.cbo+'!important}.home-lang-label{color:'+v.sfg+'!important}.command-row button,.command-row code{background:'+v.cbg+'!important;border-color:'+v.cbo+'!important;color:'+v.chf+'!important}';
	}
	function fileParts(p){var n=String(p||'').replace(/\\/g,'/'),i=n.lastIndexOf('/');return {name:i>=0?n.substring(i+1):n,path:i>=0?n.substring(0,i):''}}
	function fileType(n){var i=String(n||'').lastIndexOf('.');return i>=0?String(n).substring(i+1,i+5):'file'}
	function normalizeAttachment(x){return typeof x==='string'?{displayName:x}:x||{}}
	function isImageAttachment(x){return x&&x.imageData&&String(x.imageMediaType||'').indexOf('image/')===0}
	function imageSrc(x){return 'data:'+String(x.imageMediaType||'image/png')+';base64,'+String(x.imageData||'')}
	function attachmentHtml(items){if(!items||!items.length)return '';var rows=items.map(function(raw){var x=normalizeAttachment(raw),label=x.displayName||x.path||'',p=fileParts(label);if(isImageAttachment(x)){var src=imageSrc(x);return '<button class="u-image" data-src="'+h(src)+'" data-name="'+h(p.name||label||'Image')+'" title="'+h(label)+'"><img src="'+h(src)+'" alt="'+h(p.name||label||'Image')+'"><span class="u-image-name">'+h(p.name||label||'Image')+'</span></button>'}return '<div class="u-file" title="'+h(label)+'"><span class="u-file-icon">'+h(fileType(p.name))+'</span><span class="u-file-copy"><div class="u-file-name">'+h(p.name||label)+'</div><div class="u-file-path">'+h(p.path||'Attached file')+'</div></span></div>'}).join('');return '<div class="u-files">'+rows+'</div>'}
	function showImagePreview(src,name){var old=document.querySelector('.img-modal');if(old)old.remove();var o=document.createElement('div');o.className='img-modal';o.innerHTML='<button aria-label="Close"></button><figure><img src="'+h(src)+'" alt="'+h(name||'Image')+'"><figcaption>'+h(name||'Image')+'</figcaption></figure>';o.querySelector('button').onclick=function(){o.remove()};o.onclick=function(e){if(e.target===o)o.remove()};document.addEventListener('keydown',function esc(e){if(e.key==='Escape'){o.remove();document.removeEventListener('keydown',esc)}});document.body.appendChild(o)}
	function bindImagePreviews(root){var imgs=root.querySelectorAll('.u-image');Array.prototype.forEach.call(imgs,function(btn){btn.onclick=function(){showImagePreview(btn.getAttribute('data-src')||'',btn.getAttribute('data-name')||'Image')}})}
		function addUserMessage(t,a){clearHome();var d=document.createElement('div');d.className='um';d.innerHTML='<div class="u-card"><div class="u-text">'+h(t)+'</div>'+attachmentHtml(a)+'</div>';bindImagePreviews(d);m.appendChild(d);last=null;sd(true)}
		function beginAssistantTurn(){clearHome();ss=null;active=buildAsst('');last=active;m.appendChild(active);cv=false;sd()}
		function currentAssistant(){return active&&active.parentNode?active:null}
		function ensureAssistant(){var a=currentAssistant();if(a){last=a;return a}beginAssistantTurn();return active}
	function parts(){var a=ensureAssistant();return a.querySelector('.parts')}
	function lastBody(p){var bs=(p||parts()).querySelectorAll('.b');return bs.length?bs[bs.length-1]:null}
	function textSegment(){var p=parts(),tail=p.lastElementChild;if(tail&&tail.classList.contains('b'))return tail;var b=document.createElement('div');b.className='b';p.appendChild(b);return b}
	function addAssistantMessage(t){ss=null;var b=textSegment();b.innerHTML=md(t);enhanceMarkdown(b);renderCursor();sd()}
	function buildAsst(t){var d=document.createElement('div');d.className='am';d.innerHTML='<div class="av">AtomCode</div><div class="parts"><div class="b">'+md(t)+'</div></div>';enhanceMarkdown(d);return d}
	function removeStreamingCursors(){var olds=document.querySelectorAll('.streaming-cursor');Array.prototype.forEach.call(olds,function(x){x.remove()})}
	function renderCursor(){removeStreamingCursors();if(!last)return;var b=lastBody(last.querySelector('.parts'));if(!b)return;if(cv){var c=document.createElement('span');c.className='streaming-cursor';b.appendChild(c)}}
	function updateLastAssistantMessage(t,q){var n=Number(q||0);if(n&&n<useq)return;if(n)useq=n;var b=textSegment();ss=newStreamState(b);ss.raw=String(t||'');renderStreamState(ss,false);renderCursor();sd()}
	function appendAssistantDelta(t,q){var n=Number(q||0);if(n&&n<useq)return;if(n)useq=n;writeStreamText(currentStreamState(),t);renderCursor();sd()}
		function finishAssistantMarkdownStream(t,q){var n=Number(q||0);if(n&&n<useq)return;if(n)useq=n;if(arguments.length>0){var s=currentStreamState();s.raw=String(t||'');renderStreamState(s,true);ss=null;sd();return}if(!ss)return;if(!ss.raw){ss=null;sd();return}renderStreamState(ss,true);ss=null;sd()}
		function showStreamingCursor(){cv=true;renderCursor();sd()}
		function hideStreamingCursor(){cv=false;removeStreamingCursors();sd()}
		function finishAssistantTurn(){cv=false;removeStreamingCursors();removeThinkingIndicator();removeReasoningBlock();sd()}
	function artifactCodeBlock(id){var xs=parts().querySelectorAll('.cm[data-artifact-id]');for(var i=0;i<xs.length;i++){if(xs[i].getAttribute('data-artifact-id')===String(id))return xs[i]}return null}
	function updateArtifactCodeBlock(id,l,f,c){var e=artifactCodeBlock(id);if(!e){e=document.createElement('div');e.setAttribute('data-artifact-id',String(id));parts().appendChild(e)}var block=normalizeCodeBlock(l,c),label=f||normalizeCodeLang(block.lang);e.className='cm'+(isDiffLang(block.lang)?' cm-diff':'');if(isDiffLang(block.lang)){e.innerHTML='<div class="h"><span class="lang">'+h(label)+'</span></div><pre><span class="dl">'+diffLinesHtml(block.text)+'</span></pre>'}else{e.innerHTML='<div class="h"><span class="lang">'+h(label)+'</span></div><pre>'+h(block.text)+'</pre>'}sd()}
	function addCodeBlock(l,c,f){var d=document.createElement('div');d.className='cm';d.innerHTML='<div class="h"><span class="lang">'+h(f||l||'Code')+'</span></div><pre>'+h(c)+'</pre>';parts().appendChild(d);sd()}
	function toolTone(s){s=String(s||'').toLowerCase();return s.indexOf('error')>=0||s.indexOf('fail')>=0?'error':s.indexOf('running')>=0||s.indexOf('queued')>=0?'running':s.indexOf('done')>=0||s.indexOf('success')>=0||s.indexOf('complete')>=0?'success':'idle'}
	function toolHtml(n,s,d,a,o){var row='<summary><span class="chev">›</span><span class="tool-dot"></span><span class="tool-name">'+h(n)+'</span><span class="tool-summary">'+h(a||'')+'</span><span class="tool-status">'+h(s)+'</span></summary>';return '<details'+(o?' open':'')+'>'+row+(d?'<pre>'+h(d)+'</pre>':'')+'</details>'}
	function setTool(e,n,s,d,a,o){e.className='tm ts-'+toolTone(s);e.setAttribute('data-name',n);e.innerHTML=toolHtml(n,s,d,a,o)}
	function addToolCall(n,s,d,a){var e=document.createElement('div');setTool(e,n,s,d,a);parts().appendChild(e);sd()}
		function updateToolCall(n,s,d,a){var ps=parts();var tools=Array.prototype.slice.call(ps.querySelectorAll('.tm')).reverse();var e=tools.find(function(x){return x.getAttribute('data-name')===n})||tools[0];if(e){setTool(e,n,s,d,a);sd()}else addToolCall(n,s,d,a)}
	function addError(t){clearHome();var d=document.createElement('div');d.className='em';d.innerHTML='⚠️ '+h(t);m.appendChild(d);last=null;sd()}
function addQueuedMessage(t){clearHome();var d=document.createElement('div');d.className='qm';d.innerHTML='<span class="b">📥 '+h(t)+'</span>';m.appendChild(d);last=null;sd()}
	function addThinkingIndicator(){var d=document.createElement('div');d.className='rm thp';d.innerHTML='💭 思考中<span class="dots"></span>';parts().appendChild(d);sd()}
	function replaceThinkingWithAssistant(t){var a=ensureAssistant();var th=a.querySelector('.thp');if(th)th.remove();addAssistantMessage(t||'')}
		function removeThinkingIndicator(){var a=currentAssistant();if(a){var th=a.querySelector('.thp');if(th)th.remove()}}
	function addSystemMessage(t){clearHome();var d=document.createElement('div');d.className='sm';d.textContent=t;m.appendChild(d);last=null;sd()}
	function addAssistantEvent(t){var d=document.createElement('div');d.className='sm';d.textContent=t;parts().appendChild(d);sd()}
	function addTurnSummary(label,rounds,tools,duration,tokens,failed){var d=document.createElement('div');d.className='turn-summary'+(failed?' failed':'');d.textContent=(failed?'✗ ':'✓ ')+String(label||'Done')+' · '+Number(rounds||0)+' rounds · '+Number(tools||0)+' tools · '+String(duration||'0ms')+' · '+Number(tokens||0)+' tokens';m.appendChild(d);last=null;active=null;sd(true)}
	function reasoningPreview(t){var fl=String(t||'').split('\n')[0].substring(0,80);if(String(t||'').length>fl.length)fl+='...';return '💭 思考 — '+h(fl)}
	function addReasoningBlock(t){var p=parts(),th=p.querySelector('.thp');if(th)th.remove();var d=document.createElement('div');d.className='rm reasoning-content';d.innerHTML=reasoningPreview(t);p.insertBefore(d,p.firstChild);sd()}
	function updateReasoningBlock(t){var p=parts(),d=p.querySelector('.reasoning-content');if(!d){addReasoningBlock(t);return}d.innerHTML=reasoningPreview(t);sd()}
	function removeReasoningBlock(){var a=currentAssistant();if(!a)return;var blocks=a.querySelectorAll('.reasoning-content');Array.prototype.forEach.call(blocks,function(x){x.remove()})}
	function renderChatModel(model){clearMessages();(model.messages||[]).forEach(function(x){if(x.text!==undefined&&x.contextSummary!==undefined)addUserMessage(x.text,x.contextSummary||[]);else if(x.markdown!==undefined)addAssistantMessage(x.markdown);else if(x.toolName!==undefined)addSystemMessage('[Permission] '+x.toolName+': '+(x.reason||''));else if(x.name!==undefined&&x.callId!==undefined){var e=document.createElement('div');setTool(e,x.name,x.status||'',x.output||x.argumentsJson||'', '', false);parts().appendChild(e)}else if(x.text!==undefined)addSystemMessage(x.text)});sd()}
	function showWelcomePage(c){clearMessages();var d=document.createElement('div');d.className='home';var quick=(c.quickStart||[]).map(function(x){return '<li>'+h(x)+'</li>'}).join('');var actions=(c.actions||[]).map(function(x){return '<div class="action-row"><strong>'+h(x.name)+'</strong><span>'+h(x.label)+'</span></div>'}).join('');var commands=(c.commands||[]).map(function(x){return '<div class="command-row"><button data-action="'+h(x.action)+'">'+h(x.command)+'</button><span>'+h(x.label)+'</span></div>'}).join('');var loginBtn=c.showLogin?'<button class="home-btn" data-action="login">'+h(c.login)+'</button>':'';d.innerHTML='<section class="home-hero"><div class="home-head"><div class="home-brand"><div class="home-title"><span class="home-mark">A</span><span>'+h(c.title)+'</span></div><div class="home-subtitle">'+h(c.subtitle)+'</div></div><span class="home-lang"><span class="home-lang-label">'+h(c.languageLabel)+'</span><button class="home-btn" data-lang="zh">中文</button><button class="home-btn" data-lang="en">English</button></span></div><div class="home-actions"><button class="home-btn primary" data-action="settings">'+h(c.settings)+'</button>'+loginBtn+'<button class="home-btn" data-action="docs">'+h(c.docs)+'</button></div></section><div class="home-grid"><section class="home-section"><h2>'+h(c.quickStartTitle)+'</h2><ul>'+quick+'</ul></section><section class="home-section"><h2>'+h(c.actionsTitle)+'</h2><div class="action-list">'+actions+'</div></section><section class="home-section"><h2>'+h(c.commandsTitle)+'</h2><div class="command-list">'+commands+'</div></section><section class="home-section"><h2>'+h(c.docsTitle)+'</h2><div class="home-doc"><p>'+h(c.docsText)+'</p><button class="home-btn" data-action="docs">'+h(c.docs)+'</button></div></section></div>';m.appendChild(d);Array.prototype.forEach.call(d.querySelectorAll('[data-action]'),function(btn){btn.onclick=function(){host(btn.getAttribute('data-action')||'')}});Array.prototype.forEach.call(d.querySelectorAll('[data-lang]'),function(btn){btn.onclick=function(){switchWelcomeLanguage(btn.getAttribute('data-lang')||'en')}});sd(true)}
		function clearMessages(){m.innerHTML='';last=null;active=null;ti=-1;cv=false;nb=true;ss=null;useq=0}
(function find(){for(var k in window){if(k.indexOf('JBCefQuery_')===0&&typeof window[k]==='function'){window[k]('js:ready');return}}setTimeout(find,50)})();
</script></body></html>""".trimIndent()
    }

    private fun loadWebScript(path: String): String =
        JBCefMessageView::class.java.getResource(path)?.readText().orEmpty()
}

data class MessageAttachmentView(
    val displayName: String,
    val path: String? = null,
    val imageMediaType: String? = null,
    val imageData: String? = null,
)
