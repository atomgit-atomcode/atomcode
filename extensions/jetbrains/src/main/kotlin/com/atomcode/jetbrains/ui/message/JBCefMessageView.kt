package com.atomcode.jetbrains.ui.message

import com.intellij.ui.jcef.JBCefBrowser
import com.intellij.ui.jcef.JBCefJSQuery
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import javax.swing.JPanel
import javax.swing.SwingUtilities

/**
 * 基于 JBCefBrowser (Chromium) 的聊天消息视图。
 *
 * 架构：
 * - Kotlin → JS: browser.cefBrowser.executeJavaScript()
 * - JS → Kotlin: JBCefJSQuery 注入回调，JS 扫描 window 获取
 * - 如果 JBCefJSQuery 回调未就绪（API 废弃/兼容问题），2 秒后自动切为强制模式
 */
class JBCefMessageView : JPanel(BorderLayout()) {

    // 延迟初始化：避免在 IDE 启动早期访问 Registry (COMPONENTS_LOADED 之前)
    private var browser: JBCefBrowser? = null
    private var jsQuery: JBCefJSQuery? = null

    @Volatile private var jsReady = false
    @Volatile private var forceReady = false
    private val pendingCalls = mutableListOf<String>()
    private var initialized = false

    /**
     * addNotify 在组件被添加到可见容器时调用，此时 IDE 已完全启动。
     * 延迟创建 JBCefBrowser 避免 Registry 访问时序错误。
     */
    override fun addNotify() {
        super.addNotify()
        if (!initialized) {
            initialized = true
            initBrowser()
        }
    }

    private fun initBrowser() {
        val b = JBCefBrowser()
        browser = b
        val q = JBCefJSQuery.create(b)
        jsQuery = q

        add(b.component, BorderLayout.CENTER)

        q.addHandler { message ->
            when {
                message == "js:ready" -> {
                    jsReady = true
                    executeJs("setTheme(${isDarkTheme()})")
                    flushPending()
                    null
                }
                else -> null
            }
        }

        b.loadHTML(buildChatHtml())

        // 兜底：2 秒后如果 JS 仍未就绪，强制开始发送消息
        SwingUtilities.invokeLater {
            Thread.sleep(2000)
            if (!jsReady && !forceReady) {
                forceReady = true
                flushPending()
            }
        }
    }

    // ── Public API ──

    fun addUserMessage(text: String)            { sendJs("addUserMessage", text) }
    fun addAssistantMessage(text: String)       { sendJs("addAssistantMessage", text) }
    fun addCodeBlock(lang: String, code: String, file: String? = null) { sendJs("addCodeBlock", lang, code, file ?: "") }
    fun addToolCall(name: String, status: String, detail: String? = null) { sendJs("addToolCall", name, status, detail ?: "") }
    fun updateToolCall(name: String, status: String, detail: String? = null) { sendJs("updateToolCall", name, status, detail ?: "") }
    fun addError(text: String)                  { sendJs("addError", text) }
    fun addQueuedMessage(text: String)          { sendJs("addQueuedMessage", text) }
    fun addThinkingIndicator()                  { sendJs("addThinkingIndicator") }
    fun replaceThinkingWithAssistant(text: String) { sendJs("replaceThinkingWithAssistant", text) }
    fun removeThinkingIndicator()               { sendJs("removeThinkingIndicator") }
    fun addSystemMessage(text: String)          { sendJs("addSystemMessage", text) }
    fun addReasoningBlock(text: String)         { sendJs("addReasoningBlock", text) }
    fun updateLastAssistantMessage(text: String) { sendJs("updateLastAssistantMessage", text) }
    fun clear()                                 { sendJs("clearMessages") }

    // ── Internals ──

    private fun sendJs(fn: String, vararg args: String) {
        val escaped = args.joinToString(",") { arg ->
            "\"" + arg.replace("\\", "\\\\").replace("\"", "\\\"")
                .replace("\n", "\\n").replace("\r", "\\r") + "\""
        }
        val call = "$fn($escaped)"
        if (jsReady || forceReady) {
            executeJs(call)
        } else {
            pendingCalls.add(call)
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

    @Suppress("DEPRECATION")
    private fun isDarkTheme() = UIUtil.isUnderDarcula()

    // ── Inline HTML (避免 classpath 资源加载问题) ──

    private fun buildChatHtml(): String {
        val dark = isDarkTheme()
        val bg = if (dark) "#1e1e1e" else "#fff"
        val fg = if (dark) "#d4d4d4" else "#1e1e1e"
        val ubg = if (dark) "#094771" else "#d0e4f7"
        val ufg = if (dark) "#e0e0e0" else "#1e1e1e"
        val abg = if (dark) "#2d2d2d" else "#e8e8e8"
        val afg = if (dark) "#d4d4d4" else "#333"
        val cbg = if (dark) "#1e1e1e" else "#fafafa" // code
        val cbo = if (dark) "#3c3c3c" else "#ccc"    // code border
        val chb = if (dark) "#2d2d2d" else "#e0e0e0" // code head bg
        val chf = if (dark) "#9cdcfe" else "#005a9e" // code head fg
        val tbg = if (dark) "#1a281a" else "#e0f0e0" // tool
        val tbo = if (dark) "#2d4a2d" else "#a0c8a0"
        val tfg = if (dark) "#6a9955" else "#3d7a3d"
        val ebg = if (dark) "#3d2020" else "#f8e0e0" // error
        val ebo = if (dark) "#5a3030" else "#d8a0a0"
        val efg = if (dark) "#f48771" else "#c04040"
        val qbg = if (dark) "#1a3550" else "#e8eef4" // queued
        val qfg = if (dark) "#8899aa" else "#667788"
        val rbg = if (dark) "#1a2330" else "#f0f4f8" // reason
        val rbo = if (dark) "#2a3a4a" else "#d0d8e0"
        val sfg = if (dark) "#888" else "#666"       // system
        val vfg = if (dark) "#6a9955" else "#3d7a3d" // avatar

        return """
<!DOCTYPE html><html><head><meta charset="UTF-8"><style>
*{margin:0;padding:0;box-sizing:border-box}
body{background:$bg;color:$fg;font:13px -apple-system,'Segoe UI',sans-serif;line-height:1.5;padding:10px 12px;overflow-y:auto}
#m{display:flex;flex-direction:column;gap:8px}
.um{display:flex;justify-content:flex-end}
.um .b{background:$ubg;color:$ufg;padding:8px 14px;border-radius:12px 4px 12px 12px;max-width:78%;white-space:pre-wrap;word-break:break-word}
.am{display:flex;flex-direction:column;align-items:flex-start}
.am .av{color:$vfg;font-size:11px;margin-bottom:2px}
.am .b{background:$abg;color:$afg;padding:8px 14px;border-radius:4px 12px 12px 12px;max-width:90%;white-space:pre-wrap;word-break:break-word}
.cm{border:1px solid $cbo;border-radius:6px;overflow:hidden;background:$cbg}
.cm .h{background:$chb;color:$chf;padding:4px 10px;font-size:11px}
.cm pre{margin:0;padding:8px 12px;font:12px 'JetBrains Mono','Consolas',monospace;line-height:1.5;overflow-x:auto;white-space:pre;color:$fg}
.tm{border:1px solid $tbo;border-radius:6px;background:$tbg;color:$tfg;padding:6px 10px;font-size:12px}
.em{border:1px solid $ebo;border-radius:6px;background:$ebg;color:$efg;padding:6px 10px;font-size:12px}
.qm{display:flex;justify-content:flex-end}
.qm .b{background:$qbg;color:$qfg;padding:6px 12px;border-radius:10px 4px 10px 10px;max-width:78%;font-size:11px}
.rm{border:1px solid $rbo;border-radius:6px;background:$rbg;color:$sfg;padding:4px 10px;font-size:10px}
.sm{color:$sfg;font-size:11px}
.th{display:flex;align-items:center;gap:6px;color:$sfg;font-size:12px}
.dots::after{content:'';animation:d 1.5s steps(4,end) infinite}
@keyframes d{0%{content:''}25%{content:'.'}50%{content:'..'}75%{content:'...'}}
</style></head><body>
<div id="m"></div>
<script>
var m=document.getElementById('m'),last=null,ti=-1,nb=true;
document.body.addEventListener('scroll',function(){nb=document.body.scrollHeight-document.body.scrollTop-document.body.clientHeight<120});
function sd(){if(nb)requestAnimationFrame(function(){document.body.scrollTop=document.body.scrollHeight})}
function h(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')}
function setTheme(d){}
function addUserMessage(t){var d=document.createElement('div');d.className='um';d.innerHTML='<span class="b">'+h(t)+'</span>';m.appendChild(d);last=null;sd()}
function addAssistantMessage(t){last=buildAsst(t);m.appendChild(last);sd()}
function buildAsst(t){var d=document.createElement('div');d.className='am';d.innerHTML='<div class="av">🤖 AtomCode</div><div class="b">'+h(t)+'</div>';return d}
function updateLastAssistantMessage(t){if(last)last.querySelector('.b').textContent=t;else addAssistantMessage(t);sd()}
function addCodeBlock(l,c,f){var d=document.createElement('div');d.className='cm';d.innerHTML='<div class="h">📄 '+h(f||l||'Code')+'</div><pre>'+h(c)+'</pre>';m.appendChild(d);last=null;sd()}
function addToolCall(n,s,d){var e=document.createElement('div');e.className='tm';e.innerHTML='🔧 '+h(n)+' — '+h(s)+(d?'<br><small>'+h(d)+'</small>':'');m.appendChild(e);last=null;sd()}
function updateToolCall(n,s,d){var e=m.querySelector('.tm:last-child');if(e&&e.textContent.startsWith(n)){e.innerHTML='🔧 '+h(n)+' — '+h(s)+(d?'<br><small>'+h(d)+'</small>':'');sd()}else addToolCall(n,s,d)}
function addError(t){var d=document.createElement('div');d.className='em';d.innerHTML='⚠️ '+h(t);m.appendChild(d);last=null;sd()}
function addQueuedMessage(t){var d=document.createElement('div');d.className='qm';d.innerHTML='<span class="b">📥 '+h(t)+'</span>';m.appendChild(d);last=null;sd()}
function addThinkingIndicator(){ti=m.children.length;var d=document.createElement('div');d.className='th';d.innerHTML='<span style="color:$vfg;font-size:11px">🤖 AtomCode</span> <span>思考中<span class="dots"></span></span>';m.appendChild(d);last=null;sd()}
function replaceThinkingWithAssistant(t){if(ti>=0&&ti<m.children.length){m.removeChild(m.children[ti]);ti=-1}if(t)addAssistantMessage(t)}
function removeThinkingIndicator(){if(ti>=0&&ti<m.children.length){m.removeChild(m.children[ti]);ti=-1}}
function addSystemMessage(t){var d=document.createElement('div');d.className='sm';d.textContent=t;m.appendChild(d);last=null;sd()}
function addReasoningBlock(t){var d=document.createElement('div');d.className='rm';var fl=t.split('\n')[0].substring(0,80);if(t.length>fl.length)fl+='...';d.innerHTML='💭 思考 — '+h(fl);m.appendChild(d);last=null;sd()}
function clearMessages(){m.innerHTML='';last=null;ti=-1}
(function find(){for(var k in window){if(k.indexOf('JBCefQuery_')===0&&typeof window[k]==='function'){window[k]('js:ready');return}}setTimeout(find,50)})();
</script></body></html>""".trimIndent()
    }
}
