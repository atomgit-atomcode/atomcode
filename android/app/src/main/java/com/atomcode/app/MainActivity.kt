package com.atomcode.app

import android.annotation.SuppressLint
import android.app.AlertDialog
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.os.Environment
import android.os.IBinder
import android.provider.MediaStore
import android.provider.Settings
import android.util.Log
import java.io.File
import android.view.ViewGroup
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.core.content.FileProvider
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.webkit.WebViewAssetLoader

/**
 * The whole UI: a `WebView` pointed at the loopback server.
 *
 * That is deliberate. The front end is the existing `webui` bundle embedded in
 * the daemon (Preact + the same `dist/` a desktop browser gets), so there is no
 * second UI to keep in sync — a webui fix reaches the phone by rebuilding the
 * `.so`, and nothing here changes.
 *
 * The page needs `http://127.0.0.1:<port>/?token=...`. Cleartext to loopback is
 * allowed by `network_security_config.xml`; the token is exchanged for an
 * HttpOnly cookie by the index route, exactly as in a browser.
 */
class MainActivity : ComponentActivity() {

    private lateinit var web: WebView
    private lateinit var status: TextView
    private var loadedUrl: String? = null
    private var bound = false
    /** Where the camera app writes the picture it was asked to take, if any. */
    private var pendingCaptureFile: File? = null
    /** The `content://` URI handed to the camera for that file (granted, then revoked). */
    private var pendingCaptureUri: Uri? = null
    /** Delayed capture cleanup; the page reads the URI asynchronously. */
    private val cleanupHandler = android.os.Handler(android.os.Looper.getMainLooper())
    /** 保持 binder 引用：自愈重启后 URL 会变，回到前台要能对出来。 */
    private var binder: AgentService.LocalBinder? = null
    /** 当前连着的「外部守护进程」（adb shell 里的 atomcode daemon），null 表示用本机的。 */
    private var external: ExternalDaemon.Endpoint? = null
    /** Whether all-files access was already granted when this screen opened. */
    private var storageGrantedAtStart = false
    private var askedForStorage = false

    /**
     * One-shot request to be excluded from Doze.
     *
     * Some OEM builds throw or have no such activity at all; both are fine, the
     * click-through is a bonus on top of the JNI-driven wake lock, never a
     * precondition for a turn to finish.
     */
    private fun requestBatteryExemptionOnce() {
        val prefs = getSharedPreferences("atomcode", MODE_PRIVATE)
        if (prefs.getBoolean("battery_exemption_asked", false)) return
        val power = getSystemService(PowerManager::class.java) ?: return
        if (power.isIgnoringBatteryOptimizations(packageName)) {
            prefs.edit().putBoolean("battery_exemption_asked", true).apply()
            return
        }
        prefs.edit().putBoolean("battery_exemption_asked", true).apply()
        try {
            startActivity(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName"),
                )
            )
        } catch (failure: Exception) {
            Log.i(TAG, "battery-optimisation exemption unavailable: ${failure.message}")
        }
    }

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            bound = true
            val local = service as? AgentService.LocalBinder ?: return
            binder = local
            val url = local.url()
            if (url != null) {
                load(url)
            } else {
                showError(local.lastError())
                // 上一次启动失败时，服务自己不会重试（它只在 url 为空时尝试一次）。
                // 绑定到服务是“再试一次”的自然时机。
                AgentService.start(this@MainActivity)
            }
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            bound = false
            binder = null
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Android 13+ makes notifications a runtime permission. The foreground
        // service runs either way, but without the grant its status notification
        // is silently dropped — so the agent looks like it has no background
        // state at all. Ask once, on first launch.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), NOTIFICATION_REQUEST)
        }

        // Ask once for a battery-optimisation exemption: with it the system does
        // not Doze this app, so a turn survives a sleeping screen. Declining is a
        // supported outcome, not an error — the runtime notifies the service over
        // JNI on every turn boundary, so the wake lock still follows the work.
        requestBatteryExemptionOnce()

        // Debug builds only: lets `adb forward` + chrome://inspect drive the page
        // for on-device UI verification. Release builds keep devtools off.
        if (applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE != 0) {
            WebView.setWebContentsDebuggingEnabled(true)
        }

        val root = FrameLayout(this)
        web = WebView(this).apply {
            layoutParams = FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            settings.databaseEnabled = true
            // The webui is a local SPA; let it keep its own cookie and storage.
            settings.mediaPlaybackRequiresUserGesture = false
            // The webui opens its sign-in page with `window.open(url, '_blank')`
            // and treats a `null` return as "the browser blocked the pop-up".
            // A WebView cannot open a tab, so the page is handed to the system
            // browser instead — see `installExternalLinkBridge`.
            addJavascriptInterface(HostBridge(), "AtomCodeHost")
            webViewClient = object : WebViewClient() {
                // Every in-app navigation stays on the loopback origin. A link
                // the model emits, and the OAuth sign-in page, both leave it —
                // those go to the system browser, which is where a sign-in
                // belongs anyway (the page then polls the daemon, so no callback
                // into this app is needed).
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    val host = request.url.host ?: return true
                    if (host == "127.0.0.1" || host == "localhost") return false
                    openInBrowser(request.url.toString())
                    return true
                }

                override fun onPageFinished(view: WebView, url: String) {
                    super.onPageFinished(view, url)
                    installExternalLinkBridge(view)
                }
            }
            webChromeClient = object : WebChromeClient() {
                override fun onShowFileChooser(
                    view: WebView,
                    callback: android.webkit.ValueCallback<Array<Uri>>,
                    params: FileChooserParams,
                ): Boolean {
                    // Attachment upload from the composer. The system picker is
                    // the only way an app sandbox can hand a file to the page.
                    fileCallback = callback
                    return try {
                        startActivityForResult(chooserFor(params), FILE_CHOOSER_REQUEST)
                        true
                    } catch (e: Exception) {
                        Log.w(TAG, "no file chooser available", e)
                        fileCallback = null
                        releasePendingCapture()
                        false
                    }
                }

                override fun onConsoleMessage(message: android.webkit.ConsoleMessage): Boolean {
                    Log.d(TAG, "webui: ${message.message()} (${message.sourceId()}:${message.lineNumber()})")
                    return true
                }
            }
        }
        status = TextView(this).apply {
            text = getString(R.string.starting)
            textAlignment = TextView.TEXT_ALIGNMENT_CENTER
        }
        root.addView(web)
        root.addView(status)
        setContentView(root)

        // Android draws every app edge-to-edge since API 35, so the window
        // extends under the navigation bar (and the keyboard). The WebView
        // reports the TOP inset through `env(safe-area-inset-top)` — which the
        // webui reserves — but it reports the bottom as 0, so the composer
        // would sit under the navigation bar and the keyboard would cover it.
        //
        // Pad the WebView container for the bottom inset here, where the real
        // numbers are known, and keep the top to the page's own
        // `env(safe-area-inset-top)`. The keyboard is the taller of the two, so
        // the composer stays reachable while typing.
        ViewCompat.setOnApplyWindowInsetsListener(root) { view, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
            view.setPadding(bars.left, 0, bars.right, maxOf(bars.bottom, ime.bottom))
            insets
        }

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                // The webui is an SPA with its own in-page navigation; back goes
                // through its history first, and only then leaves the app.
                if (web.canGoBack()) web.goBack() else {
                    isEnabled = false
                    onBackPressedDispatcher.onBackPressed()
                }
            }
        })

        AgentService.start(this)
        bindService(Intent(this, AgentService::class.java), connection, Context.BIND_AUTO_CREATE)
        storageGrantedAtStart = Environment.isExternalStorageManager()
        maybeAttachExternal()
    }

    override fun onResume() {
        super.onResume()
        // The helper can arrive by several routes — our own install prompt, a
        // file manager, `adb install` — and none of them tells us reliably: the
        // installer's activity result is not delivered at all on some OEMs
        // (measured on ColorOS), and a one-shot flag gets consumed by an
        // `onResume` that fires before the package is actually registered. So
        // the state is *compared* instead of remembered.
        reconcileHelperLinks()
        val granted = Environment.isExternalStorageManager()
        if (granted && !storageGrantedAtStart) {
            // The agent's workspace just moved from the app sandbox to shared
            // storage. That root is chosen once, at server start, so the server
            // has to be restarted for the change to take effect — the session
            // log on disk is untouched.
            storageGrantedAtStart = true
            AgentService.stop(this)
            AgentService.start(this)
            recreate()
            return
        }
        if (!granted && !askedForStorage) {
            askedForStorage = true
            promptStorageAccess()
        }
        // 自愈重启会换端口：界面还连着旧端口时，所有请求都会失败（页面是缓存里的）。
        // 回到前台时对一下当前服务的 URL，变了就重载 —— `load` 自己会跳过相同值。
        // 外部守护进程不在我们这个进程里，没人替它看活：回前台确认一次，断了就回落。
        external?.let { endpoint ->
            if (!ExternalDaemon.isAlive(endpoint.port)) {
                useInProcess(getString(R.string.external_lost))
            }
        }
        binder?.url()?.let { current -> load(current) }
    }

    /**
     * Ask for all-files access, once, with an explanation.
     *
     * Without it the agent still runs, but only inside its private directory —
     * useful for trying things out, useless for opening the repository the
     * person actually wants to work on.
     */
    private fun promptStorageAccess() {
        AlertDialog.Builder(this)
            .setTitle(getString(R.string.storage_title))
            .setMessage(getString(R.string.storage_message))
            .setPositiveButton(getString(R.string.storage_grant)) { _, _ ->
                val app = Intent(
                    Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
                    Uri.parse("package:$packageName"),
                )
                try {
                    startActivity(app)
                } catch (e: Exception) {
                    Log.w(TAG, "per-app storage settings unavailable, opening the list", e)
                    startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
                }
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    /** What the page may ask the host to do. Loopback page only. */
    inner class HostBridge {
        @android.webkit.JavascriptInterface
        fun openExternal(url: String) {
            runOnUiThread { openInBrowser(url) }
        }

        /**
         * Which provider APKs (`com.atomcode.tools`, `com.atomcode.tools.zig`, …) are
         * installed right now, as a JSON array of package ids.
         *
         * The panel needs this per provider: the market publishes one row per APK, and
         * "installed" for `zig` is a different package than "installed" for `make`.
         * A daemon-side answer could not say it — the daemon sees `<data>/bin` links,
         * not packages — so the state comes from the host that has `PackageManager`.
         *
         * Synchronous on purpose: the bridge call is on the WebView thread and a
         * `PackageManager` lookup is cheap; an async answer would leave the panel
         * rendering a "checking…" state for something that is already known.
         */
        @android.webkit.JavascriptInterface
        fun installedProviders(): String {
            return AgentService.HELPER_PACKAGES.mapNotNull { name ->
                try {
                    packageManager.getApplicationInfo(name, 0)
                    name
                } catch (missing: android.content.pm.PackageManager.NameNotFoundException) {
                    null
                }
            }.joinToString(prefix = "[", postfix = "]", separator = ",") { "\"$it\"" }
        }

        /**
         * Start the bundled CLI as an external daemon and switch to it.
         *
         * Returns nothing: the daemon needs seconds to answer `/health`, and this
         * runs on the WebView's bridge thread — a boolean here would always be a
         * lie (the same trap `installHelper` had). The outcome arrives as the
         * usual switch, or as a toast carrying the reason it failed.
         *
         * No confirmation dialog on purpose: the person just asked for it.
         */
        @android.webkit.JavascriptInterface
        fun startExternalDaemon() {
            Thread {
                val failure = ExternalDaemonLauncher.start(this@MainActivity)
                if (failure != null) {
                    Log.w(TAG, "external daemon did not start: $failure")
                    runOnUiThread { toast(failure) }
                    return@Thread
                }
                val trusted = ExternalDaemon.searchDirs(File(filesDir, "atomcode"))
                val hint = ExternalDaemon.searchDirs(
                    File(Environment.getExternalStorageDirectory(), SHARED_DIR),
                )
                val found = ExternalDaemon.discover(trusted, hint)
                runOnUiThread {
                    val endpoint = found.endpoint
                    if (endpoint == null) {
                        toast(getString(R.string.external_start_unseen))
                    } else {
                        useExternal(endpoint)
                    }
                }
            }.start()
        }

        /** Stop the daemon this app started, and fall back to the in-app agent. */
        @android.webkit.JavascriptInterface
        fun stopExternalDaemon() {
            Thread {
                ExternalDaemonLauncher.stop()
                runOnUiThread { if (external != null) useInProcess(null) }
            }.start()
        }

        /**
         * Whether the session log is kept app-private right now.
         *
         * Synchronous on purpose: it is a single file existence check, so the
         * page can render the toggle state without a round trip.
         */
        @android.webkit.JavascriptInterface
        fun sessionsPrivate(): Boolean = SessionsLocation.isPrivate(this@MainActivity)

        /**
         * Move the session log into (or out of) app-private storage.
         *
         * The new location only applies to the *next* process start — the host
         * pins the session root once, before the runtime exists — so this tells
         * the person to reopen the app rather than pretending it took effect.
         */
        @android.webkit.JavascriptInterface
        fun setSessionsPrivate(private: Boolean) {
            SessionsLocation.setPrivate(this@MainActivity, private)
            runOnUiThread {
                toast(getString(if (private) R.string.sessions_private_on else R.string.sessions_private_off))
            }
        }

        /**
         * Install the helper APK the daemon downloaded.
         *
         * The file lives in this app's own storage, and the system installer
         * cannot read a `file://` URI from another process — so it gets a
         * `content://` one from our own FileProvider, scoped to that one
         * directory.
         *
         * **Returns nothing on purpose.** This method runs on the WebView's
         * bridge thread, so `runOnUiThread` has not executed yet when it
         * returns — a boolean here would always be `false` and the page showed
         * "no installer available" even while the prompt was on screen. The
         * outcome is not knowable at this point anyway: the person installs in
         * the system UI, and the page learns the result by asking for the
         * helper status again.
         */
        @android.webkit.JavascriptInterface
        fun installHelper(path: String) {
            runOnUiThread { installHelperApk(path) }
        }
    }

    /**
     * Whether the optional helper APK is installed right now.
     *
     * Uses the package manager rather than the `<data>/bin` links: the links
     * are written by the host at start-up, so right after an install they are
     * exactly what is still missing. The package is the truth.
     */
    private fun helperInstalled(): Boolean = AgentService.HELPER_PACKAGES.any { name ->
        try {
            packageManager.getApplicationInfo(name, 0)
            true
        } catch (missing: android.content.pm.PackageManager.NameNotFoundException) {
            false
        }
    }

    /**
     * Make the helper's state and its `<data>/bin` links agree.
     *
     * The links are written by the host at start-up, so whenever the helper is
     * installed or removed while the app is alive they go stale. Restarting is
     * the only way to re-link, and it is safe: the session log is durable, so a
     * restart resumes rather than loses.
     *
     * `make` is the probe because every helper build ships it — unlike
     * `atomcode`, which only newer helpers carry, and comparing against a link
     * that may legitimately be missing would restart on every resume.
     */
    private fun reconcileHelperLinks() {
        val installed = helperInstalled()
        val linked = File(filesDir, "atomcode/bin/make").exists()
        if (installed == linked) return
        Log.i(
            TAG,
            "helper state changed (installed=$installed, linked=$linked); restarting the agent",
        )
        restartAgent()
        if (installed) toast(getString(R.string.helper_installed))
    }

    /**
     * Hand the helper APK to the system package installer.
     *
     * The APK must sit inside the directory our FileProvider publishes
     * (`files/atomcode/helper`); a path outside it cannot be shared, and the
     * installer would be handed a URI it cannot read.
     */
    private fun installHelperApk(path: String): Boolean {
        val file = File(path)
        if (!file.isFile) {
            Log.w(TAG, "helper APK is missing: $path")
            return false
        }
        val uri = try {
            androidx.core.content.FileProvider.getUriForFile(this, "$packageName.files", file)
        } catch (failure: Exception) {
            Log.w(TAG, "cannot share the helper APK", failure)
            return false
        }
        val intent = Intent(Intent.ACTION_INSTALL_PACKAGE).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        return try {
            // No activity result is awaited: the installer's verdict is not
            // delivered on some OEMs, and `onResume` reconciles the state
            // anyway — which also covers installs that arrive by other routes.
            startActivity(intent)
            true
        } catch (failure: Exception) {
            Log.w(TAG, "no package installer available", failure)
            false
        }
    }

    /**
     * Make `window.open(external)` reach the system browser and report success.
     *
     * Returning an object (rather than `null`) matters: the webui reads a `null`
     * return as a blocked pop-up and shows "the browser blocked the sign-in
     * page". The object also gives it something to set `opener = null` on, which
     * is the part of `noopener` the page cares about.
     *
     * Idempotent — injected on every page load because the SPA may be reloaded.
     */
    private fun installExternalLinkBridge(view: WebView) {
        val script = """
            (function () {
              if (window.__atomcodeHostBridge) return;
              window.__atomcodeHostBridge = true;
              var original = window.open;
              window.open = function (url, target, features) {
                try {
                  if (url && /^https?:/i.test(String(url))) {
                    AtomCodeHost.openExternal(String(url));
                    return { opener: null, closed: false, focus: function () {}, close: function () {} };
                  }
                } catch (e) { /* fall through to the real open */ }
                return original ? original.call(window, url, target, features) : null;
              };
            })();
        """.trimIndent()
        view.evaluateJavascript(script, null)
    }

    /** Hand an external URL to whichever app handles it (browser, usually). */
    private fun openInBrowser(url: String) {
        if (!url.startsWith("http://") && !url.startsWith("https://")) {
            Log.w(TAG, "refusing to open non-http URL: $url")
            return
        }
        try {
            startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            })
        } catch (e: Exception) {
            Log.e(TAG, "no app can open $url", e)
            status.visibility = android.view.View.VISIBLE
            status.text = getString(R.string.no_browser)
        }
    }

    /**
     * 尝试连上外部守护进程（`adb shell` 里跑的 `atomcode daemon`）。
     *
     * 发现只能间接做：Android 不允许应用枚举其它 uid 的进程，所以靠共享目录里的
     * 汇合文件 `daemon-<port>.json`（给出端口与 token）加 `/health` 指纹（证明它
     * 确实在跑、而且是我们的）。询问只做一轮：拒绝过的端口不再打扰，端口变了再问。
     */
    private fun maybeAttachExternal() {
        val prefs = getSharedPreferences("atomcode", MODE_PRIVATE)
        val shared = File(Environment.getExternalStorageDirectory(), SHARED_DIR)
        // 私有目录里的汇合文件可以给 token；共享存储里的只当端口线索（谁都能读）。
        val trusted = ExternalDaemon.searchDirs(File(filesDir, "atomcode"))
        val hint = ExternalDaemon.searchDirs(shared)
        Thread {
            val found = ExternalDaemon.discover(trusted, hint)
            runOnUiThread { handleDiscovery(found) }
        }.start()
    }

    /** 把发现结果翻译成一次询问：能直接连的，或只拿到“要 token”的端口。 */
    private fun handleDiscovery(found: ExternalDaemon.Discovery) {
        val prefs = getSharedPreferences("atomcode", MODE_PRIVATE)
        val endpoint = found.endpoint
        if (endpoint != null) {
            if (endpoint.port == prefs.getInt("external_declined_port", 0)) return
            if (endpoint.port == prefs.getInt("external_port", 0)) {
                // 上次就已经选过它：直接用，不再问第二遍。
                useExternal(endpoint)
            } else {
                askAboutExternal(endpoint)
            }
            return
        }
        val port = found.needsTokenPort ?: return
        if (port == prefs.getInt("external_token_declined_port", 0)) return
        askForToken(port)
    }

    /**
     * 共享存储里发现了 daemon，但它要 token 才肯干活。
     *
     * 凭据不从共享存储取（那里的文件谁都能读），所以只有两条路：用户手输，或以
     * `--no-auth` 重起。输过一次就不再为同一个端口打扰。
     */
    private fun askForToken(port: Int) {
        val input = android.widget.EditText(this).apply {
            hint = getString(R.string.external_token_hint)
            setSingleLine()
        }
        AlertDialog.Builder(this)
            .setTitle(getString(R.string.external_token_title))
            .setMessage(getString(R.string.external_token_message, port))
            .setView(input)
            .setPositiveButton(getString(R.string.external_token_connect)) { _, _ ->
                val token = input.text.toString().trim()
                if (token.isEmpty()) return@setPositiveButton
                useExternal(ExternalDaemon.Endpoint(port, "http://127.0.0.1:$port/?token=$token"))
            }
            .setNegativeButton(getString(R.string.external_local)) { _, _ ->
                getSharedPreferences("atomcode", MODE_PRIVATE).edit()
                    .putInt("external_token_declined_port", port)
                    .apply()
            }
            .show()
    }

    private fun askAboutExternal(endpoint: ExternalDaemon.Endpoint) {
        AlertDialog.Builder(this)
            .setTitle(getString(R.string.external_title))
            .setMessage(getString(R.string.external_message, endpoint.port))
            .setPositiveButton(getString(R.string.external_switch)) { _, _ -> useExternal(endpoint) }
            .setNegativeButton(getString(R.string.external_local)) { _, _ ->
                getSharedPreferences("atomcode", MODE_PRIVATE).edit()
                    .putInt("external_declined_port", endpoint.port)
                    .apply()
            }
            .show()
    }

    private fun useExternal(endpoint: ExternalDaemon.Endpoint) {
        external = endpoint
        getSharedPreferences("atomcode", MODE_PRIVATE).edit()
            .putInt("external_port", endpoint.port)
            .remove("external_declined_port")
            .apply()
        // 前台服务留着（通知、唤醒锁、忙碌统计都还要它），但只陪跑外部 daemon；
        // 进程内 daemon 单独停掉：两份运行时同时写一份会话日志会互相打架。
        AgentService.attachExternal(this, endpoint.port)
        NativeAgent.nativeStop()
        if (bound) {
            unbindService(connection)
            bound = false
            binder = null
        }
        status.visibility = android.view.View.GONE
        load(endpoint.url)
        toast(getString(R.string.external_connected, endpoint.port))
    }

    /** 切回进程内守护进程。`reason` 非空时先说明为什么。 */
    private fun useInProcess(reason: String?) {
        external = null
        getSharedPreferences("atomcode", MODE_PRIVATE).edit().remove("external_port").apply()
        reason?.let { toast(it) }
        // 由服务自己清掉外部模式并重起进程内 daemon（URL 会换，绑定回调会重载）。
        AgentService.detachExternal(this)
        if (!bound) {
            bindService(Intent(this, AgentService::class.java), connection, Context.BIND_AUTO_CREATE)
        }
    }

    private fun toast(text: String) {
        android.widget.Toast.makeText(this, text, android.widget.Toast.LENGTH_LONG).show()
    }

    private fun load(url: String) {
        if (loadedUrl == url) return
        loadedUrl = url
        status.visibility = android.view.View.GONE
        web.loadUrl(url)
    }

    private fun showError(message: String?) {
        status.visibility = android.view.View.VISIBLE
        status.text = getString(R.string.start_failed, message ?: "unknown")
    }

    override fun onDestroy() {
        if (bound) {
            unbindService(connection)
            bound = false
        }
        web.destroy()
        super.onDestroy()
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != FILE_CHOOSER_REQUEST) return
        val callback = fileCallback ?: return
        fileCallback = null
        val captured = pendingCaptureFile
        val capturedBytes = captured?.takeIf { it.exists() }?.length() ?: -1L
        val uris = when {
            resultCode != RESULT_OK -> null
            data?.clipData != null -> Array(data.clipData!!.itemCount) { data.clipData!!.getItemAt(it).uri }
            data?.data != null -> arrayOf(data.data!!)
            // 相机走 `EXTRA_OUTPUT`：结果里不带 URI，图直接写在预置的文件里。
            captured != null && capturedBytes > 0 ->
                arrayOf(FileProvider.getUriForFile(this, "$packageName.files", captured))
            // Some OEM cameras ignore `EXTRA_OUTPUT` and hand back a (downscaled) JPEG
            // in the intent extras instead — measured on OPPO's camera, whose save path
            // answers "open sharedFd failed: Permission denied" for a foreign provider.
            // That is still the picture the person took, so keep it.
            else -> saveCaptureFromExtras(data, captured)
        }
        Log.i(
            TAG,
            "file chooser result code=$resultCode uris=${uris?.size ?: 0} " +
                "captureBytes=$capturedBytes extras=${data?.hasExtra("data") == true}",
        )
        releasePendingCapture()
        callback.onReceiveValue(uris)
        // The WebView reads the URI ASYNCHRONOUSLY. Deleting the file here — which is
        // what this used to do — made every capture fail with ENOENT in
        // `cr_ContentUriUtils`, while the file itself looked fine on disk.
        if (captured != null && capturedBytes > 0) deleteCaptureLater(captured)
    }

    /** Give up our claim on the capture (grant and bookkeeping), not the bytes. */
    private fun releasePendingCapture() {
        val uri = pendingCaptureUri
        pendingCaptureUri = null
        pendingCaptureFile = null
        if (uri != null) {
            // The grant was given to a named camera package; it must not outlive the
            // hand-off.
            revokeUriPermission(
                uri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
            )
        }
    }

    /** Remove one scratch capture once the page has had time to read it. */
    private fun deleteCaptureLater(file: File) {
        cleanupHandler.postDelayed({ file.delete() }, CAPTURE_LINGER_MS)
    }

    /**
     * Delete captures from earlier sessions.
     *
     * A capture the page never read (the person backed out of the camera) would
     * otherwise stay in the cache forever; anything older than a few minutes is
     * certainly done with.
     */
    private fun pruneStaleCaptures(dir: File) {
        val cutoff = System.currentTimeMillis() - STALE_CAPTURE_MS
        dir.listFiles()?.forEach { file ->
            if (file.isFile && file.lastModified() < cutoff) file.delete()
        }
    }

    /**
     * Last resort for a camera that returns the picture in the result extras.
     *
     * Writes it over the scratch file we already advertised, so the page receives a
     * normal `content://` URI and everything downstream (compression, thumbnails) is
     * the same path as a picked image.
     */
    private fun saveCaptureFromExtras(data: Intent?, captured: File?): Array<Uri>? {
        val target = captured ?: return null
        val bitmap = try {
            @Suppress("DEPRECATION")
            data?.getParcelableExtra<android.graphics.Bitmap>("data")
        } catch (e: Exception) {
            Log.w(TAG, "camera extras are unreadable", e)
            null
        } ?: return null
        return try {
            java.io.FileOutputStream(target).use { stream ->
                bitmap.compress(android.graphics.Bitmap.CompressFormat.JPEG, 92, stream)
            }
            bitmap.recycle()
            arrayOf(FileProvider.getUriForFile(this, "$packageName.files", target))
        } catch (e: Exception) {
            Log.w(TAG, "could not keep the camera's extra picture", e)
            null
        }
    }

    /**
     * 「拍照」与「选文件」并列的文件选择框。
     *
     * `WebChromeClient.onShowFileChooser` 给的是系统文件选择器（相册/下载），它没有
     * 相机入口——想拍一张就得先退出应用。`ACTION_IMAGE_CAPTURE` 作为 chooser 的
     * initial intent 挂上去，两个入口就在同一屏里。相机只接受一个输出 URI，所以
     * 多选在拍照这条路上退化成一张（用户本来也只会拍一张）。
     */
    private fun chooserFor(params: WebChromeClient.FileChooserParams): Intent {
        val picker = params.createIntent()
        val capture = cameraCaptureIntent() ?: return picker
        val grant = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        val chooser = Intent.createChooser(picker, getString(R.string.attach_choose))
        chooser.putExtra(Intent.EXTRA_INITIAL_INTENTS, arrayOf(capture))
        // A chooser does not hand URI grants down to its EXTRA_INITIAL_INTENTS on
        // every OEM build: measured here, OPPO's camera logged `open sharedFd failed
        // with Permission denied` and the WebView then got ENOENT on the empty file.
        // So grant to the camera package by name and carry the flags on the chooser.
        chooser.addFlags(grant)
        val target = pendingCaptureUri
        if (target != null) {
            // Grant to EVERY app that can answer: a chooser does not hand URI grants
            // down to its EXTRA_INITIAL_INTENTS on every OEM build, and restricting to
            // MATCH_DEFAULT_ONLY already missed at least one camera here.
            val cameras = packageManager
                .queryIntentActivities(capture, 0)
                .mapNotNull { it.activityInfo?.packageName }
                .distinct()
            for (camera in cameras) {
                try {
                    grantUriPermission(camera, target, grant)
                } catch (e: Exception) {
                    Log.w(TAG, "could not grant the capture URI to $camera", e)
                }
            }
            Log.i(TAG, "camera capture granted to ${cameras.joinToString(",").ifEmpty { "no app" }}")
        }
        return chooser
    }

    /** `ACTION_IMAGE_CAPTURE`，输出落在 cache/captures/ 的 FileProvider URI 上。 */
    private fun cameraCaptureIntent(): Intent? {
        val dir = File(cacheDir, "captures").apply { mkdirs() }
        pruneStaleCaptures(dir)
        val target = File(dir, "capture-${System.currentTimeMillis()}.jpg")
        val uri = try {
            FileProvider.getUriForFile(this, "$packageName.files", target)
        } catch (e: Exception) {
            // 没有相机应用、或 provider 未配置：退回纯文件选择器，不拦住附件。
            Log.w(TAG, "camera capture is unavailable", e)
            return null
        }
        pendingCaptureFile = target
        pendingCaptureUri = uri
        return Intent(MediaStore.ACTION_IMAGE_CAPTURE).apply {
            putExtra(MediaStore.EXTRA_OUTPUT, uri)
            addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION or Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
    }

    /**
     * Restart the in-process agent.
     *
     * Used when the helper APK arrives: its binaries are linked at host
     * start-up, so the running server has to come back for `make`/`tcc`/`zig`
     * to be on `PATH`.
     */
    private fun restartAgent() {
        NativeAgent.nativeStop()
        bound = false
        AgentService.stop(this)
        AgentService.start(this)
    }

    companion object {
        private const val TAG = "AtomCode"
        /** 共享存储下 agent 的工作目录；外部 daemon 把 ATOMACODE_HOME 指到这里。 */
        private const val SHARED_DIR = "AtomCode"
        private const val FILE_CHOOSER_REQUEST = 0x51
        /** How long a camera capture stays readable after it is handed to the page. */
        private const val CAPTURE_LINGER_MS = 60_000L
        /** Captures untouched for this long are leftovers from a cancelled camera. */
        private const val STALE_CAPTURE_MS = 10 * 60_000L
        /** Runtime notification prompt (Android 13+). */
        private const val NOTIFICATION_REQUEST = 0x52
        private var fileCallback: android.webkit.ValueCallback<Array<Uri>>? = null
    }
}