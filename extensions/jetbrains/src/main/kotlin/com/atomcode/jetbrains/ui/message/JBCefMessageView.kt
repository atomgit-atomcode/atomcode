package com.atomcode.jetbrains.ui.message

import com.google.gson.Gson
import com.intellij.ui.jcef.JBCefBrowser
import com.intellij.ui.jcef.JBCefJSQuery
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import javax.swing.JPanel
import javax.swing.Timer

/**
 * 基于 JBCefBrowser (Chromium) 的聊天消息视图。
 *
 * 架构：
 * - Kotlin → JS: browser.cefBrowser.executeJavaScript()
 * - JS → Kotlin: JBCefJSQuery 注入回调，JS 扫描 window 获取
 * - 如果 JBCefJSQuery 回调未就绪（API 废弃/兼容问题），2 秒后自动切为强制模式
 */
class JBCefMessageView : JPanel(BorderLayout()) {
    private val gson = Gson()

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
            scheduleBrowserInit()
        }
    }

    private fun scheduleBrowserInit() {
        Timer(300) {
            if (browser == null && isDisplayable) {
                initBrowser()
            }
        }.apply {
            isRepeats = false
            start()
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

        // 兜底：2 秒后如果 JS 仍未就绪，强制开始发送消息。
        Timer(2000) {
            if (!jsReady && !forceReady) {
                forceReady = true
                flushPending()
            }
        }.apply {
            isRepeats = false
            start()
        }
    }

    // ── Public API ──

    fun addUserMessage(text: String)            { sendJs("addUserMessage", text) }
    fun beginAssistantTurn()                    { sendJs("beginAssistantTurn") }
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
    fun addAssistantEvent(text: String)         { sendJs("addAssistantEvent", text) }
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
        if (jsReady || forceReady) {
            executeJs(call)
        } else {
            pendingCalls.add(call)
        }
    }

    private fun sendRawJs(call: String) {
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
        val markedScript = loadWebScript("/markdown/marked.min.js")
        val purifyScript = loadWebScript("/markdown/purify.min.js")
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
	.am .parts{display:flex;flex-direction:column;gap:8px;align-items:flex-start;width:100%}
	.am .b{background:$abg;color:$afg;padding:8px 14px;border-radius:4px 12px 12px 12px;max-width:90%;white-space:normal;word-break:break-word}
	.am .b:empty{display:none}
	.am .b h1,.am .b h2,.am .b h3,.am .b h4{margin:12px 0 6px;line-height:1.3}
	.am .b h1{font-size:1.45em}.am .b h2{font-size:1.3em}.am .b h3{font-size:1.15em}
	.am .b p{margin:6px 0}
	.am .b ul,.am .b ol{margin:6px 0;padding-left:24px}
	.am .b blockquote{margin:8px 0;padding-left:10px;border-left:3px solid $cbo;color:$sfg}
	.am .b code{font:12px 'JetBrains Mono','Consolas',monospace;background:$cbg;border-radius:3px;padding:1px 4px}
	.am .b pre{margin:8px 0;padding:10px 12px;overflow-x:auto;white-space:pre;background:$cbg;border:1px solid $cbo;border-radius:6px}
	.am .b pre code{padding:0;background:transparent}
	.am .b table{border-collapse:collapse;margin:8px 0;max-width:100%;display:block;overflow-x:auto}
	.am .b th,.am .b td{border:1px solid $cbo;padding:5px 8px;text-align:left}
	.am .b a{color:$chf}
	.am .b>:first-child{margin-top:0}.am .b>:last-child{margin-bottom:0}
.cm{border:1px solid $cbo;border-radius:6px;overflow:hidden;background:$cbg}
.cm .h{background:$chb;color:$chf;padding:4px 10px;font-size:11px}
.cm pre{margin:0;padding:8px 12px;font:12px 'JetBrains Mono','Consolas',monospace;line-height:1.5;overflow-x:auto;white-space:pre;color:$fg}
	.tm{border:1px solid $tbo;border-radius:6px;background:$tbg;color:$tfg;padding:6px 10px;font-size:12px}
	.tm summary{cursor:pointer;list-style:none}
	.tm summary::-webkit-details-marker{display:none}
	.tm pre{margin:6px 0 0 0;max-height:260px;overflow:auto;white-space:pre-wrap;word-break:break-word;color:$fg;background:$cbg;border:1px solid $cbo;border-radius:4px;padding:6px 8px}
.em{border:1px solid $ebo;border-radius:6px;background:$ebg;color:$efg;padding:6px 10px;font-size:12px}
.qm{display:flex;justify-content:flex-end}
.qm .b{background:$qbg;color:$qfg;padding:6px 12px;border-radius:10px 4px 10px 10px;max-width:78%;font-size:11px}
.rm{border:1px solid $rbo;border-radius:6px;background:$rbg;color:$sfg;padding:4px 10px;font-size:10px;max-width:90%;margin-bottom:4px}
.sm{color:$sfg;font-size:11px}
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
		var m=document.getElementById('m'),last=null,active=null,ti=-1,nb=true,cv=false;
document.body.addEventListener('scroll',function(){nb=document.body.scrollHeight-document.body.scrollTop-document.body.clientHeight<120});
function sd(){if(nb)requestAnimationFrame(function(){document.body.scrollTop=document.body.scrollHeight})}
function h(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')}
	function md(s){
		var source=String(s||'');
		if(typeof marked==='undefined'||typeof DOMPurify==='undefined')return h(source).replace(/\n/g,'<br>');
		return DOMPurify.sanitize(marked.parse(source,{gfm:true,breaks:true}),{USE_PROFILES:{html:true}})
	}
	function setTheme(d){}
		function addUserMessage(t){var d=document.createElement('div');d.className='um';d.innerHTML='<span class="b">'+h(t)+'</span>';m.appendChild(d);last=null;sd()}
		function beginAssistantTurn(){active=buildAsst('');last=active;m.appendChild(active);cv=false;sd()}
		function currentAssistant(){return active&&active.parentNode?active:null}
		function ensureAssistant(){var a=currentAssistant();if(a){last=a;return a}beginAssistantTurn();return active}
	function parts(){var a=ensureAssistant();return a.querySelector('.parts')}
	function lastBody(p){var bs=(p||parts()).querySelectorAll('.b');return bs.length?bs[bs.length-1]:null}
	function textSegment(){var p=parts(),tail=p.lastElementChild;if(tail&&tail.classList.contains('b'))return tail;var b=document.createElement('div');b.className='b';p.appendChild(b);return b}
	function addAssistantMessage(t){var b=textSegment();b.innerHTML=md(t);renderCursor();sd()}
	function buildAsst(t){var d=document.createElement('div');d.className='am';d.innerHTML='<div class="av">🤖 AtomCode</div><div class="parts"><div class="b">'+md(t)+'</div></div>';return d}
	function renderCursor(){if(!last)return;var olds=last.querySelectorAll('.streaming-cursor');Array.prototype.forEach.call(olds,function(x){x.remove()});var b=lastBody(last.querySelector('.parts'));if(!b)return;if(cv){var c=document.createElement('span');c.className='streaming-cursor';b.appendChild(c)}}
	function updateLastAssistantMessage(t){var b=textSegment();b.innerHTML=md(t);renderCursor();sd()}
		function showStreamingCursor(){cv=true;renderCursor();sd()}
		function hideStreamingCursor(){cv=false;renderCursor();sd()}
		function finishAssistantTurn(){cv=false;hideStreamingCursor();removeThinkingIndicator();removeReasoningBlock()}
	function addCodeBlock(l,c,f){var d=document.createElement('div');d.className='cm';d.innerHTML='<div class="h">📄 '+h(f||l||'Code')+'</div><pre>'+h(c)+'</pre>';parts().appendChild(d);sd()}
	function toolHtml(n,s,d){var summary='🔧 '+h(n)+' — '+h(s);return d?'<details open><summary>'+summary+'</summary><pre>'+h(d)+'</pre></details>':summary}
	function addToolCall(n,s,d){var e=document.createElement('div');e.className='tm';e.setAttribute('data-name',n);e.innerHTML=toolHtml(n,s,d);parts().appendChild(e);sd()}
		function updateToolCall(n,s,d){var ps=parts();var tools=Array.prototype.slice.call(ps.querySelectorAll('.tm')).reverse();var e=tools.find(function(x){return x.getAttribute('data-name')===n})||tools[0];if(e){e.setAttribute('data-name',n);e.innerHTML=toolHtml(n,s,d);sd()}else addToolCall(n,s,d)}
function addError(t){var d=document.createElement('div');d.className='em';d.innerHTML='⚠️ '+h(t);m.appendChild(d);last=null;sd()}
function addQueuedMessage(t){var d=document.createElement('div');d.className='qm';d.innerHTML='<span class="b">📥 '+h(t)+'</span>';m.appendChild(d);last=null;sd()}
	function addThinkingIndicator(){var d=document.createElement('div');d.className='rm thp';d.innerHTML='💭 思考中<span class="dots"></span>';parts().appendChild(d);sd()}
	function replaceThinkingWithAssistant(t){var a=ensureAssistant();var th=a.querySelector('.thp');if(th)th.remove();addAssistantMessage(t||'')}
		function removeThinkingIndicator(){var a=currentAssistant();if(a){var th=a.querySelector('.thp');if(th)th.remove()}}
	function addSystemMessage(t){var d=document.createElement('div');d.className='sm';d.textContent=t;m.appendChild(d);last=null;sd()}
	function addAssistantEvent(t){var d=document.createElement('div');d.className='sm';d.textContent=t;parts().appendChild(d);sd()}
	function reasoningPreview(t){var fl=String(t||'').split('\n')[0].substring(0,80);if(String(t||'').length>fl.length)fl+='...';return '💭 思考 — '+h(fl)}
	function addReasoningBlock(t){var p=parts(),th=p.querySelector('.thp');if(th)th.remove();var d=document.createElement('div');d.className='rm reasoning-content';d.innerHTML=reasoningPreview(t);p.insertBefore(d,p.firstChild);sd()}
	function updateReasoningBlock(t){var p=parts(),d=p.querySelector('.reasoning-content');if(!d){addReasoningBlock(t);return}d.innerHTML=reasoningPreview(t);sd()}
	function removeReasoningBlock(){var a=currentAssistant();if(!a)return;var blocks=a.querySelectorAll('.reasoning-content');Array.prototype.forEach.call(blocks,function(x){x.remove()})}
	function renderChatModel(model){clearMessages();(model.messages||[]).forEach(function(x){if(x.text!==undefined&&x.contextSummary!==undefined)addUserMessage(x.text);else if(x.markdown!==undefined)addAssistantMessage(x.markdown);else if(x.toolName!==undefined)addSystemMessage('[Permission] '+x.toolName+': '+(x.reason||''));else if(x.name!==undefined&&x.callId!==undefined)addToolCall(x.name,x.status||'',x.output||x.argumentsJson||'');else if(x.text!==undefined)addSystemMessage(x.text)});sd()}
		function clearMessages(){m.innerHTML='';last=null;active=null;ti=-1;cv=false}
(function find(){for(var k in window){if(k.indexOf('JBCefQuery_')===0&&typeof window[k]==='function'){window[k]('js:ready');return}}setTimeout(find,50)})();
</script></body></html>""".trimIndent()
    }

    private fun loadWebScript(path: String): String =
        JBCefMessageView::class.java.getResource(path)?.readText().orEmpty()
}
