package com.atomcode.jetbrains.ui.message

import com.google.gson.Gson
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.editor.colors.EditorColorsManager
import com.intellij.ui.JBColor
import com.intellij.ui.jcef.JBCefApp
import com.intellij.ui.jcef.JBCefBrowser
import com.intellij.ui.jcef.JBCefBrowserBase
import com.intellij.ui.jcef.JBCefJSQuery
import com.intellij.util.ui.UIUtil
import org.cef.browser.CefBrowser
import org.cef.browser.CefFrame
import org.cef.handler.CefLoadHandlerAdapter
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
class JBCefMessageView : JPanel(BorderLayout()) {
    private val gson = Gson()
    private data class BrowserTheme(val dark: Boolean, val bg: String, val fg: String)

    // 延迟初始化：只在组件进入可显示层级后创建 JCEF 浏览器。
    private var browser: JBCefBrowser? = null
    private var jsQuery: JBCefJSQuery? = null

    @Volatile private var jsReady = false
    private val pendingCalls = mutableListOf<String>()
    private var initialized = false

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
            if (browser == null && isDisplayable) {
                initBrowser()
            }
        }, ModalityState.nonModal())
    }

    private fun initBrowser() {
        if (!JBCefApp.isSupported()) {
            showBrowserUnavailable()
            return
        }

        val b = JBCefBrowser()
        browser = b
        val q = JBCefJSQuery.create(b as JBCefBrowserBase)
        jsQuery = q

        add(b.component, BorderLayout.CENTER)

        q.addHandler { message ->
            when {
                message == "js:ready" -> {
                    markJsReady()
                    null
                }
                else -> null
            }
        }

        b.jbCefClient.addLoadHandler(object : CefLoadHandlerAdapter() {
            override fun onLoadEnd(browser: CefBrowser?, frame: CefFrame?, httpStatusCode: Int) {
                if (frame?.isMain == true) {
                    markJsReady()
                }
            }
        }, b.cefBrowser)
        b.loadHTML(buildChatHtml())
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
        jsQuery?.dispose()
        jsQuery = null
        browser?.dispose()
        browser = null
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
    fun updateLastAssistantMessage(text: String) { sendJs("updateLastAssistantMessage", text) }
    fun showStreamingCursor()                   { sendJs("showStreamingCursor") }
    fun hideStreamingCursor()                   { sendJs("hideStreamingCursor") }
    fun finishAssistantTurn()                   { sendJs("finishAssistantTurn") }
    fun clear()                                 { sendJs("clearMessages") }
    fun render(model: ChatRenderModel)          { sendRawJs("renderChatModel(${gson.toJson(model)})") }

    // ── Internals ──

    private fun sendJs(fn: String, vararg args: String) {
        val escaped = args.joinToString(",") { arg ->
            "\"" + arg.replace("\\", "\\\\").replace("\"", "\\\"")
                .replace("\n", "\\n").replace("\r", "\\r") + "\""
        }
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
        if (browser == null) return
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
        val b = browser ?: return
        try {
            b.cefBrowser.executeJavaScript(code, b.cefBrowser.url, 0)
        } catch (_: Exception) { }
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
.cm{border:1px solid $cbo;border-radius:7px;overflow:hidden;background:$cbg;margin:2px 0}
.cm .h{background:$chb;color:$chf;padding:5px 10px;font-size:11px;border-bottom:1px solid $cbo}
.cm pre{margin:0;padding:9px 12px;font:12px 'JetBrains Mono','Consolas',monospace;line-height:1.55;overflow-x:auto;white-space:pre;color:$fg}
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
</style></head><body>
<div id="m"></div>
<script>$markedScript</script>
<script>$purifyScript</script>
<script>
		var m=document.getElementById('m'),last=null,active=null,ti=-1,nb=true,cv=false,sr=0;
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
	function md(s){
		var source=String(s||'');
		if(typeof marked==='undefined'||typeof DOMPurify==='undefined')return h(source).replace(/\n/g,'<br>');
		return DOMPurify.sanitize(marked.parse(source,{gfm:true,breaks:true}),{USE_PROFILES:{html:true}})
	}
	function tv(d){return d?{
		bg:'#1e1e1e',fg:'#d4d4d4',ubg:'#094771',ufg:'#e0e0e0',ub:'#245b82',afg:'#d4d4d4',
		cbg:'#1e1e1e',cbo:'#3c3c3c',chb:'#2d2d2d',chf:'#9cdcfe',tbg:'#252526',tbo:'#3c3c3c',
		tfg:'#a7a7a7',ebg:'#3d2020',ebo:'#5a3030',efg:'#f48771',qbg:'#1a3550',qfg:'#8899aa',
		rbg:'#1a2330',rbo:'#2a3a4a',sfg:'#888',vfg:'#8fbc72',avbg:'#293424',fileBg:'rgba(255,255,255,.055)',
		fileIconBg:'#21405a',fileIconFg:'#9bd3f5'
	}:{
		bg:'#ffffff',fg:'#1e1e1e',ubg:'#d0e4f7',ufg:'#1e1e1e',ub:'#b8d3e7',afg:'#333333',
		cbg:'#fafafa',cbo:'#cccccc',chb:'#e0e0e0',chf:'#005a9e',tbg:'#f4f4f4',tbo:'#d8d8d8',
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
		'.cm .h{background:'+v.chb+'!important;color:'+v.chf+'!important;border-bottom-color:'+v.cbo+'!important}.cm pre{color:'+v.fg+'!important}'+
		'.tm{color:'+v.tfg+'!important}.tm details[open],.tm summary:hover{background:'+v.tbg+'!important;border-color:'+v.tbo+'!important;color:'+v.fg+'!important}'+
		'.tm .chev,.tm .tool-dot,.tm .tool-summary,.tm .tool-status{color:'+v.sfg+'!important}.tm .tool-name,.tm pre{color:'+v.fg+'!important}.tm pre{background:'+v.cbg+'!important;border-top-color:'+v.tbo+'!important}'+
		'.em{background:'+v.ebg+'!important;border-color:'+v.ebo+'!important;color:'+v.efg+'!important}.qm .b{background:'+v.qbg+'!important;color:'+v.qfg+'!important}'+
		'.rm{background:'+v.rbg+'!important;border-left-color:'+v.rbo+'!important;color:'+v.sfg+'!important}.sm,.th,.turn-summary{color:'+v.sfg+'!important}.turn-summary:before,.turn-summary:after{background:'+v.cbo+'!important}.turn-summary.failed{color:'+v.efg+'!important}.turn-summary.failed:before,.turn-summary.failed:after{background:'+v.ebo+'!important}.streaming-cursor{background:'+v.afg+'!important}';
	}
	function fileParts(p){var n=String(p||'').replace(/\\/g,'/'),i=n.lastIndexOf('/');return {name:i>=0?n.substring(i+1):n,path:i>=0?n.substring(0,i):''}}
	function fileType(n){var i=String(n||'').lastIndexOf('.');return i>=0?String(n).substring(i+1,i+5):'file'}
	function normalizeAttachment(x){return typeof x==='string'?{displayName:x}:x||{}}
	function isImageAttachment(x){return x&&x.imageData&&String(x.imageMediaType||'').indexOf('image/')===0}
	function imageSrc(x){return 'data:'+String(x.imageMediaType||'image/png')+';base64,'+String(x.imageData||'')}
	function attachmentHtml(items){if(!items||!items.length)return '';var rows=items.map(function(raw){var x=normalizeAttachment(raw),label=x.displayName||x.path||'',p=fileParts(label);if(isImageAttachment(x)){var src=imageSrc(x);return '<button class="u-image" data-src="'+h(src)+'" data-name="'+h(p.name||label||'Image')+'" title="'+h(label)+'"><img src="'+h(src)+'" alt="'+h(p.name||label||'Image')+'"><span class="u-image-name">'+h(p.name||label||'Image')+'</span></button>'}return '<div class="u-file" title="'+h(label)+'"><span class="u-file-icon">'+h(fileType(p.name))+'</span><span class="u-file-copy"><div class="u-file-name">'+h(p.name||label)+'</div><div class="u-file-path">'+h(p.path||'Attached file')+'</div></span></div>'}).join('');return '<div class="u-files">'+rows+'</div>'}
	function showImagePreview(src,name){var old=document.querySelector('.img-modal');if(old)old.remove();var o=document.createElement('div');o.className='img-modal';o.innerHTML='<button aria-label="Close"></button><figure><img src="'+h(src)+'" alt="'+h(name||'Image')+'"><figcaption>'+h(name||'Image')+'</figcaption></figure>';o.querySelector('button').onclick=function(){o.remove()};o.onclick=function(e){if(e.target===o)o.remove()};document.addEventListener('keydown',function esc(e){if(e.key==='Escape'){o.remove();document.removeEventListener('keydown',esc)}});document.body.appendChild(o)}
	function bindImagePreviews(root){var imgs=root.querySelectorAll('.u-image');Array.prototype.forEach.call(imgs,function(btn){btn.onclick=function(){showImagePreview(btn.getAttribute('data-src')||'',btn.getAttribute('data-name')||'Image')}})}
		function addUserMessage(t,a){var d=document.createElement('div');d.className='um';d.innerHTML='<div class="u-card"><div class="u-text">'+h(t)+'</div>'+attachmentHtml(a)+'</div>';bindImagePreviews(d);m.appendChild(d);last=null;sd(true)}
		function beginAssistantTurn(){active=buildAsst('');last=active;m.appendChild(active);cv=false;sd()}
		function currentAssistant(){return active&&active.parentNode?active:null}
		function ensureAssistant(){var a=currentAssistant();if(a){last=a;return a}beginAssistantTurn();return active}
	function parts(){var a=ensureAssistant();return a.querySelector('.parts')}
	function lastBody(p){var bs=(p||parts()).querySelectorAll('.b');return bs.length?bs[bs.length-1]:null}
	function textSegment(){var p=parts(),tail=p.lastElementChild;if(tail&&tail.classList.contains('b'))return tail;var b=document.createElement('div');b.className='b';p.appendChild(b);return b}
	function addAssistantMessage(t){var b=textSegment();b.innerHTML=md(t);renderCursor();sd()}
	function buildAsst(t){var d=document.createElement('div');d.className='am';d.innerHTML='<div class="av">AtomCode</div><div class="parts"><div class="b">'+md(t)+'</div></div>';return d}
	function removeStreamingCursors(){var olds=document.querySelectorAll('.streaming-cursor');Array.prototype.forEach.call(olds,function(x){x.remove()})}
	function renderCursor(){removeStreamingCursors();if(!last)return;var b=lastBody(last.querySelector('.parts'));if(!b)return;if(cv){var c=document.createElement('span');c.className='streaming-cursor';b.appendChild(c)}}
	function updateLastAssistantMessage(t){var b=textSegment();b.innerHTML=md(t);renderCursor();sd()}
		function showStreamingCursor(){cv=true;renderCursor();sd()}
		function hideStreamingCursor(){cv=false;removeStreamingCursors();sd()}
		function finishAssistantTurn(){cv=false;removeStreamingCursors();removeThinkingIndicator();removeReasoningBlock();sd()}
	function addCodeBlock(l,c,f){var d=document.createElement('div');d.className='cm';d.innerHTML='<div class="h">📄 '+h(f||l||'Code')+'</div><pre>'+h(c)+'</pre>';parts().appendChild(d);sd()}
	function toolTone(s){s=String(s||'').toLowerCase();return s.indexOf('error')>=0||s.indexOf('fail')>=0?'error':s.indexOf('running')>=0||s.indexOf('queued')>=0?'running':s.indexOf('done')>=0||s.indexOf('success')>=0||s.indexOf('complete')>=0?'success':'idle'}
	function toolHtml(n,s,d,a,o){var row='<summary><span class="chev">›</span><span class="tool-dot"></span><span class="tool-name">'+h(n)+'</span><span class="tool-summary">'+h(a||'')+'</span><span class="tool-status">'+h(s)+'</span></summary>';return '<details'+(o?' open':'')+'>'+row+(d?'<pre>'+h(d)+'</pre>':'')+'</details>'}
	function setTool(e,n,s,d,a,o){e.className='tm ts-'+toolTone(s);e.setAttribute('data-name',n);e.innerHTML=toolHtml(n,s,d,a,o)}
	function addToolCall(n,s,d,a){var e=document.createElement('div');setTool(e,n,s,d,a);parts().appendChild(e);sd()}
		function updateToolCall(n,s,d,a){var ps=parts();var tools=Array.prototype.slice.call(ps.querySelectorAll('.tm')).reverse();var e=tools.find(function(x){return x.getAttribute('data-name')===n})||tools[0];if(e){setTool(e,n,s,d,a);sd()}else addToolCall(n,s,d,a)}
function addError(t){var d=document.createElement('div');d.className='em';d.innerHTML='⚠️ '+h(t);m.appendChild(d);last=null;sd()}
function addQueuedMessage(t){var d=document.createElement('div');d.className='qm';d.innerHTML='<span class="b">📥 '+h(t)+'</span>';m.appendChild(d);last=null;sd()}
	function addThinkingIndicator(){var d=document.createElement('div');d.className='rm thp';d.innerHTML='💭 思考中<span class="dots"></span>';parts().appendChild(d);sd()}
	function replaceThinkingWithAssistant(t){var a=ensureAssistant();var th=a.querySelector('.thp');if(th)th.remove();addAssistantMessage(t||'')}
		function removeThinkingIndicator(){var a=currentAssistant();if(a){var th=a.querySelector('.thp');if(th)th.remove()}}
	function addSystemMessage(t){var d=document.createElement('div');d.className='sm';d.textContent=t;m.appendChild(d);last=null;sd()}
	function addAssistantEvent(t){var d=document.createElement('div');d.className='sm';d.textContent=t;parts().appendChild(d);sd()}
	function addTurnSummary(label,rounds,tools,duration,tokens,failed){var d=document.createElement('div');d.className='turn-summary'+(failed?' failed':'');d.textContent=(failed?'✗ ':'✓ ')+String(label||'Done')+' · '+Number(rounds||0)+' rounds · '+Number(tools||0)+' tools · '+String(duration||'0ms')+' · '+Number(tokens||0)+' tokens';m.appendChild(d);last=null;active=null;sd(true)}
	function reasoningPreview(t){var fl=String(t||'').split('\n')[0].substring(0,80);if(String(t||'').length>fl.length)fl+='...';return '💭 思考 — '+h(fl)}
	function addReasoningBlock(t){var p=parts(),th=p.querySelector('.thp');if(th)th.remove();var d=document.createElement('div');d.className='rm reasoning-content';d.innerHTML=reasoningPreview(t);p.insertBefore(d,p.firstChild);sd()}
	function updateReasoningBlock(t){var p=parts(),d=p.querySelector('.reasoning-content');if(!d){addReasoningBlock(t);return}d.innerHTML=reasoningPreview(t);sd()}
	function removeReasoningBlock(){var a=currentAssistant();if(!a)return;var blocks=a.querySelectorAll('.reasoning-content');Array.prototype.forEach.call(blocks,function(x){x.remove()})}
	function renderChatModel(model){clearMessages();(model.messages||[]).forEach(function(x){if(x.text!==undefined&&x.contextSummary!==undefined)addUserMessage(x.text,x.contextSummary||[]);else if(x.markdown!==undefined)addAssistantMessage(x.markdown);else if(x.toolName!==undefined)addSystemMessage('[Permission] '+x.toolName+': '+(x.reason||''));else if(x.name!==undefined&&x.callId!==undefined){var e=document.createElement('div');setTool(e,x.name,x.status||'',x.output||x.argumentsJson||'', '', false);parts().appendChild(e)}else if(x.text!==undefined)addSystemMessage(x.text)});sd()}
		function clearMessages(){m.innerHTML='';last=null;active=null;ti=-1;cv=false;nb=true}
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
