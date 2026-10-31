package com.atomcode.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Binder
import android.os.Environment
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import org.json.JSONArray
import org.json.JSONObject

/**
 * Keeps the agent alive while the screen is off.
 *
 * The agent is a local HTTP server plus a Rust runtime. Android freezes a cached
 * process within seconds of the screen going off, which would abort a turn
 * mid-flight, so the server lives inside a foreground service. A partial wake
 * lock is held only while a turn is active (or the activity state is unknown),
 * rather than for the entire lifetime of an idle server.
 *
 * The service holds no agent state of its own: everything that matters is in
 * [NativeAgent] (one Rust runtime per process) and on disk under `filesDir`.
 */
class AgentService : Service() {

    /** What `/health` said last: work in progress, plus how the last finished turn ended. */
    private data class DaemonStatus(
        val activeTurns: Int,
        val lastTurns: List<LastTurnResult>,
    )

    /**
     * One conversation's last FINISHED turn (`last_turns[]` in `/health`).
     *
     * [outcome] is `ok` / `failed` / `cancelled`; [stopReason] is the daemon's wire
     * name for why it ended; [summary] is a short excerpt of the final assistant text
     * (already collapsed and capped by the daemon); [sessionId] says which
     * conversation it belongs to, so two turns running at once are two lines.
     */
    private data class LastTurnResult(
        val outcome: String,
        val stopReason: String,
        val summary: String?,
        val sessionId: String?,
        val finishedAtUnix: Long,
    )

    inner class LocalBinder : Binder() {
        fun url(): String? = urlOrError.first
        fun lastError(): String? = urlOrError.second
    }

    private val binder = LocalBinder()
    private var wakeLock: PowerManager.WakeLock? = null
    private var statusExecutor: ScheduledExecutorService? = null
    private val statusLock = Any()
    @Volatile private var acceptingStatus = false
    /** The runtime says a turn is running (JNI hook, authoritative). */
    @Volatile private var turnActive = false
    /** What the last health poll saw, as a fallback before the hook fires. */
    @Volatile private var busyFromPoll = 0
    /** 连续多少次 /health 失败才判定服务器已死并重启（5 秒一轮，3 轮≈15 秒）。 */
    private var healthFailures = 0
    /** 非 0 表示前台服务只在“陪跑”外部 daemon（端口），进程内 daemon 已停。 */
    @Volatile private var externalPort = 0
    /** Where the status poller asks: needed again to resolve session titles on demand. */
    @Volatile private var statusPort = 0
    @Volatile private var statusToken: String? = null
    /**
     * `session id → title`, read from `/sessions` only when a NEW finished turn shows
     * up. Titles live in the session catalog (the webui's source of truth too); the
     * notification asks for them once instead of the daemon reading its index every
     * two seconds.
     */
    @Volatile private var sessionTitles: Map<String, String> = emptyMap()
    private var sessionTitlesFetchedFor = -1L

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        Log.i(TAG, "agent service created")
        NativeAgent.nativeInit(Log.INFO)
        AgentPower.bind(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(NOTIFICATION_ID, buildNotification())
        acquireWakeLock()
        when (intent?.action) {
            ACTION_ATTACH_EXTERNAL -> {
                val port = intent.getIntExtra(EXTRA_EXTERNAL_PORT, 0)
                if (port > 0) {
                    Log.i(TAG, "following the external daemon on port $port")
                    externalPort = port
                    healthFailures = 0
                    // 进程内 daemon 由调用方停掉（JNI）：两份运行时同时写同一份会话
                    // 日志会互相打架。这里只留前台服务与轮询。
                    startStatusPolling("http://127.0.0.1:$port/")
                }
            }
            ACTION_DETACH_EXTERNAL -> {
                Log.i(TAG, "leaving external mode")
                externalPort = 0
                stopStatusPolling()
                urlOrError = null to null
                startAgent()
            }
            else -> {
                if (externalPort == 0 && urlOrError.first == null) {
                    startAgent()
                }
            }
        }
        // START_STICKY: if the OS kills us for memory, bring the server back.
        // The session log is already durable, so a restart resumes, not loses.
        return START_STICKY
    }

    /** 外部 daemon 掉线时，让通知说实话（不重启本机的，等界面回前台判断）。 */
    private fun publishExternalLost() {
        getSystemService(NotificationManager::class.java).notify(
            NOTIFICATION_ID,
            buildNotification(getString(R.string.notification_external_lost, externalPort)),
        )
    }

    /** 停掉状态轮询（自愈重启、切外部/回本机都要先停干净）。 */
    private fun stopStatusPolling() {
        synchronized(statusLock) {
            acceptingStatus = false
            statusExecutor?.shutdownNow()
            statusExecutor = null
        }
    }

    /**
     * Where the optional helper APK keeps its binaries, or null if absent.
     *
     * The helper (`com.atomcode.tools`) carries tools that need to spawn child
     * processes — `make` runs a compiler, a compiler runs an assembler. Those
     * cannot live in this app: Android forbids executing anything from our own
     * data directory. It does allow executing *another* package's
     * `nativeLibraryDir` (measured on device), so the helper just holds files
     * and this app points `<data>/bin` links at them. No service, no IPC.
     *
     * The lookup needs `<queries>` in the manifest: since Android 11 an app can
     * only see packages it declares interest in, and without it this throws
     * `NameNotFoundException` even when the helper is installed.
     */
    /** Every installed provider APK's `nativeLibraryDir`, joined by `\n`. */
    private fun helperLibraryDirs(): String {
        val dirs = HELPER_PACKAGES.mapNotNull { name ->
            try {
                val info = packageManager.getApplicationInfo(name, 0)
                val dir = File(info.nativeLibraryDir)
                if (dir.isDirectory) dir.absolutePath else null
            } catch (missing: android.content.pm.PackageManager.NameNotFoundException) {
                null
            }
        }
        return dirs.joinToString("\n")
    }

    private fun startAgent() {
        val dataDir = File(filesDir, "atomcode").apply { mkdirs() }
        val workspace = chooseWorkspace(dataDir)
        val nativeLibDir = applicationInfo.nativeLibraryDir
        val helperLibDirs = helperLibraryDirs()
        Log.i(TAG, "workspace: ${workspace.absolutePath}")
        Log.i(TAG, "nativeLibraryDir: $nativeLibDir")
        Log.i(TAG, "helperLibraryDirs: ${helperLibDirs.ifEmpty { "(none installed)" }}")

        val url = NativeAgent.nativeStart(
            dataDir.absolutePath,
            workspace.absolutePath,
            nativeLibDir,
            helperLibDirs.ifEmpty { null },
        )
        urlOrError = if (url != null) {
            Log.i(TAG, "agent server listening at $url")
            startStatusPolling(url)
            url to null
        } else {
            val error = NativeAgent.nativeLastError() ?: "unknown error"
            Log.e(TAG, "agent server failed to start: $error")
            null to error
        }
    }

    /**
     * Where the agent may work.
     *
     * A coding agent needs real POSIX paths: `bash`, `git` and every LSP open
     * files by path, and SAF's `content://` URIs are useless to them. So when
     * the person has granted all-files access, the agent starts in a visible
     * folder on shared storage — the place a Git pull or a file manager can
     * also reach. Without the grant it stays inside the app sandbox, which is
     * private but still a working directory.
     */
    private fun chooseWorkspace(dataDir: File): File {
        val shared = if (Environment.isExternalStorageManager()) {
            File(Environment.getExternalStorageDirectory(), SHARED_DIR).apply { mkdirs() }
        } else {
            null
        }
        return shared?.takeIf { it.canWrite() }
            ?: File(dataDir, "projects/default").apply { mkdirs() }
    }

    /**
     * The daemon said a turn started (or the last one ended).
     *
     * This is the authoritative signal for keeping the CPU awake. The health
     * poll still runs — it rewrites the notification text — but it can no longer
     * be the thing that decides the lock: once the device sleeps, the poller
     * does not run at all.
     */
    fun onTurnActive(active: Boolean) {
        synchronized(statusLock) {
            turnActive = active
            if (active) {
                acquireWakeLock()
            } else if (busyFromPoll == 0) {
                releaseWakeLock()
            }
        }
    }

    private fun acquireWakeLock() {
        if (wakeLock?.isHeld == true) return
        val power = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "atomcode:agent").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseWakeLock() {
        wakeLock?.takeIf { it.isHeld }?.release()
    }

    private fun buildNotification(
        text: CharSequence = getString(R.string.notification_starting),
        detail: String? = null,
        lines: List<String> = emptyList(),
    ): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, getString(R.string.channel_agent), NotificationManager.IMPORTANCE_LOW)
            )
        }
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.notification_title))
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentIntent(open)
            .setOngoing(true)
        when {
            // 折叠态只有一行（系统会截断长文本），展开态才列出每个会话的结果。
            lines.isNotEmpty() -> {
                val inbox = Notification.InboxStyle().setBigContentTitle(text)
                lines.take(MAX_NOTIFICATION_LINES).forEach { inbox.addLine(it) }
                builder.setStyle(inbox)
            }
            detail != null -> builder.setStyle(Notification.BigTextStyle().bigText("$text\n$detail"))
        }
        return builder.build()
    }

    /** Show what the daemon is actually doing and keep the CPU awake only for work.
     *
     * A single fixed-delay worker prevents overlapping health requests. Failure
     * conservatively retains the wake lock: only an authoritative zero from both
     * execution paths may release it. */
    private fun startStatusPolling(url: String) {
        // 自愈重启会换端口，旧的轮询必须换掉；否则它一直问已经死掉的那个端口。
        synchronized(statusLock) {
            acceptingStatus = false
            statusExecutor?.shutdownNow()
            statusExecutor = null
        }
        val parsed = android.net.Uri.parse(url)
        val port = parsed.port
        val token = parsed.getQueryParameter("token")
        if (port <= 0) return
        statusPort = port
        statusToken = token
        // A new process (or a restarted daemon) has new session ids; titles from the
        // previous generation would be wrong.
        sessionTitles = emptyMap()
        sessionTitlesFetchedFor = -1L
        acceptingStatus = true
        statusExecutor = Executors.newSingleThreadScheduledExecutor().also { executor ->
            // 固定间隔做不到退避：一轮结束后再决定下一次多久（有活快问、闲着慢问）。
            lateinit var tick: () -> Unit

            fun scheduleNext() {
                // 自愈重启会换执行器；旧的那个不该再排下一轮。
                if (!acceptingStatus || statusExecutor !== executor) return
                val busy = turnActive || busyFromPoll > 0 || healthFailures > 0
                val delay = if (busy) ACTIVE_INTERVAL_MS else IDLE_INTERVAL_MS
                try {
                    executor.schedule({ tick() }, delay, TimeUnit.MILLISECONDS)
                } catch (replaced: java.util.concurrent.RejectedExecutionException) {
                    // 轮询已经被换掉了，不用再排。
                }
            }

            tick = {
                if (acceptingStatus) {
                    val status = fetchStatus(port, token)
                    if (status == null) {
                        if (externalPort != 0) {
                            // 外部 daemon 不归我们重启：通知说实话，回落由界面回前台决定。
                            healthFailures += 1
                            if (healthFailures == HEALTH_FAILURES_BEFORE_RESTART) {
                                Log.w(TAG, "external daemon on port $externalPort stopped answering")
                                publishExternalLost()
                            }
                        } else {
                            // 服务器不再应答：daemon 任务自己退了（`/shutdown`，或
                            // run_server 出错）。Kotlin 侧只缓存了 URL，不查活就会一直把
                            // 它当“在跑”，于是每次启动都跳过 startAgent —— 页面是缓存的，
                            // 功能全死。
                            healthFailures += 1
                            if (healthFailures >= HEALTH_FAILURES_BEFORE_RESTART && acceptingStatus) {
                                Log.w(
                                    TAG,
                                    "agent server stopped answering after $healthFailures polls; restarting",
                                )
                                healthFailures = 0
                                NativeAgent.nativeStop()
                                urlOrError = null to null
                                startAgent()
                            }
                        }
                    } else {
                        healthFailures = 0
                        synchronized(statusLock) {
                            if (acceptingStatus) publishStatus(status)
                        }
                    }
                }
                scheduleNext()
            }

            // 第一轮很快跑，之后由 scheduleNext() 决定节奏。
            executor.schedule({ tick() }, ACTIVE_INTERVAL_MS, TimeUnit.MILLISECONDS)
        }
    }

    /** Running `/chat` plus native live turns and the last finished turn, or `null`
     *  when the daemon did not answer. */
    private fun fetchStatus(port: Int, token: String?): DaemonStatus? {
        return try {
            val connection = (java.net.URL("http://127.0.0.1:$port/health").openConnection()
                as java.net.HttpURLConnection).apply {
                connectTimeout = 3000
                readTimeout = 3000
                if (token != null) setRequestProperty("Authorization", "Bearer $token")
            }
            try {
                if (connection.responseCode != 200) return null
                val body = connection.inputStream.bufferedReader().use { it.readText() }
                // 200 就说明它在跑。缺字段（外部实现、或老版本 daemon）按“空闲 / 没有
                // 上一轮”算，不要当成“没应答”——那会让通知谎称守护进程掉线，也会触发
                // 不必要的重启。不是 JSON 也同理：那是别人的东西在这个端口上应答。
                val json = runCatching { JSONObject(body) }.getOrNull()
                    ?: return DaemonStatus(0, emptyList())
                val active = json.optInt("active_chats", 0) + json.optInt("active_live", 0)
                val turns = json.optJSONArray("last_turns")
                val lastTurns = buildList {
                    for (index in 0 until (turns?.length() ?: 0)) {
                        val turn = turns?.optJSONObject(index) ?: continue
                        add(
                            LastTurnResult(
                                outcome = turn.optString("outcome", ""),
                                stopReason = turn.optString("stop_reason", ""),
                                summary = turn.optString("summary", "").ifEmpty { null },
                                sessionId = turn.optString("session_id", "").ifEmpty { null },
                                finishedAtUnix = turn.optLong("finished_at_unix", 0L),
                            ),
                        )
                    }
                }
                DaemonStatus(active, lastTurns)
            } finally {
                connection.disconnect()
            }
        } catch (failure: Exception) {
            null
        }
    }

    private fun publishStatus(status: DaemonStatus) {
        val activeTurns = status.activeTurns
        synchronized(statusLock) {
            busyFromPoll = activeTurns
            // The JNI hook owns the lock. The poll may only *add* to it: a turn
            // it can see is work worth staying awake for, while "0" must not
            // release a lock the runtime is holding for a turn that just began.
            if (activeTurns > 0 || turnActive) acquireWakeLock() else releaseWakeLock()
        }
        val turns = if (activeTurns == 0 && externalPort == 0) status.lastTurns else emptyList()
        val text = when {
            activeTurns > 0 -> getString(R.string.notification_busy, activeTurns)
            // 外部模式下没有本地运行时，说清“现在是谁在干活”。
            externalPort != 0 -> getString(R.string.notification_external, externalPort)
            // 两个以上会话都有结果时，一条折叠文案说不清是谁；展开列表逐行给出。
            turns.size > 1 -> getString(
                R.string.notification_multiple_results,
                getString(R.string.notification_idle),
                turns.size,
            )
            else -> singleTurnText(turns.firstOrNull())
        }
        val lines = if (turns.size > 1) {
            val titles = titlesFor(turns)
            turns.map { turn ->
                val label = titles[turn.sessionId] ?: shortSession(turn.sessionId)
                "$label · ${resultLabel(turn)}"
            }
        } else {
            emptyList()
        }
        // The expanded view carries the model's own last words; the collapsed line
        // stays short because the system truncates anything longer.
        val detail = turns.singleOrNull()?.takeIf { it.outcome == OUTCOME_OK }?.summary
        val manager = getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID, buildNotification(text, detail, lines))
    }

    private fun singleTurnText(turn: LastTurnResult?): String = when (turn?.outcome) {
        OUTCOME_OK -> getString(R.string.notification_last_turn_ok, getString(R.string.notification_idle))
        OUTCOME_CANCELLED -> getString(
            R.string.notification_last_turn_cancelled,
            getString(R.string.notification_idle),
        )
        OUTCOME_FAILED -> getString(
            R.string.notification_last_turn_failed,
            getString(R.string.notification_idle),
            turn.stopReason.ifEmpty { "error" },
        )
        else -> getString(R.string.notification_idle)
    }

    /** One conversation's result, as a word — the line already names the session. */
    private fun resultLabel(turn: LastTurnResult): String = when (turn.outcome) {
        OUTCOME_OK -> getString(R.string.notification_result_ok)
        OUTCOME_CANCELLED -> getString(R.string.notification_result_cancelled)
        OUTCOME_FAILED -> getString(
            R.string.notification_result_failed,
            turn.stopReason.ifEmpty { "error" },
        )
        else -> turn.outcome.ifEmpty { "?" }
    }

    private fun shortSession(sessionId: String?): String =
        if (sessionId.isNullOrEmpty()) {
            getString(R.string.notification_session_unknown)
        } else {
            "#" + sessionId.take(6)
        }

    /**
     * Titles for the conversations that just finished, fetched once per new result.
     *
     * The session catalog is the webui's source of truth for these names too, and a
     * poll every two seconds must not re-read it: only a NEWER `finished_at_unix`
     * triggers a fetch, and a failed fetch leaves the previous map (a short id is a
     * better badge than a second request every tick).
     */
    private fun titlesFor(turns: List<LastTurnResult>): Map<String, String> {
        val newest = turns.maxOfOrNull { it.finishedAtUnix } ?: 0L
        if (newest > sessionTitlesFetchedFor) {
            val fetched = fetchSessionTitles()
            if (fetched.isNotEmpty()) {
                sessionTitles = fetched
                sessionTitlesFetchedFor = newest
            }
        }
        return sessionTitles
    }

    /** `GET /sessions` → `id → title`. Empty when the catalog cannot be read. */
    private fun fetchSessionTitles(): Map<String, String> {
        val port = statusPort
        if (port <= 0) return emptyMap()
        return try {
            val connection = (java.net.URL("http://127.0.0.1:$port/sessions").openConnection()
                as java.net.HttpURLConnection).apply {
                connectTimeout = 3000
                readTimeout = 3000
                statusToken?.let { setRequestProperty("Authorization", "Bearer $it") }
            }
            try {
                if (connection.responseCode != 200) return emptyMap()
                val body = connection.inputStream.bufferedReader().use { it.readText() }
                val entries = runCatching { JSONArray(body) }.getOrNull() ?: return emptyMap()
                buildMap {
                    for (index in 0 until entries.length()) {
                        val entry = entries.optJSONObject(index) ?: continue
                        val id = entry.optString("id", "")
                        val name = entry.optString("name", "")
                        if (id.isNotEmpty() && name.isNotEmpty()) put(id, name)
                    }
                }
            } finally {
                connection.disconnect()
            }
        } catch (failure: Exception) {
            emptyMap()
        }
    }

    override fun onDestroy() {
        Log.i(TAG, "agent service destroyed")
        AgentPower.unbind(this)
        synchronized(statusLock) {
            acceptingStatus = false
            statusExecutor?.shutdownNow()
            statusExecutor = null
        }
        NativeAgent.nativeStop()
        externalPort = 0
        urlOrError = null to null
        releaseWakeLock()
        wakeLock = null
        super.onDestroy()
    }

    companion object {
        private const val TAG = "AtomCode"
        private const val CHANNEL_ID = "atomcode-agent"
        private const val NOTIFICATION_ID = 1
        /** Lines the expanded notification lists before the system truncates them. */
        private const val MAX_NOTIFICATION_LINES = 4
        /** `/health`'s `last_turns[].outcome` values, as `turn_outcome` serializes them. */
        private const val OUTCOME_OK = "ok"
        private const val OUTCOME_FAILED = "failed"
        private const val OUTCOME_CANCELLED = "cancelled"
        /** Folder under shared storage the agent starts in once access is granted. */
        private const val SHARED_DIR = "AtomCode"
        /** The optional helper APK; `MainActivity` checks it after an install. */
        internal const val HELPER_PACKAGE = "com.atomcode.tools"
        /**
         * Packages whose `nativeLibraryDir` this app may execute.
         *
         * `tools` holds `make`/`tcc`/the CLI; `tools.zig` holds zig, which was 141 MB
         * of the helper's 82 MB APK and now ships on its own (its standard library is
         * the separate `zig-lib` package). Both are plain file holders — the platform
         * forbids executing our own data directory, so an APK directory is the only
         * place a process-model tool can live. Each needs a `<queries>` entry.
         */
        internal val HELPER_PACKAGES = listOf("com.atomcode.tools", "com.atomcode.tools.zig")
        /**
         * 空闲时多久问一次 daemon。
         *
         * 原来的 5 秒固定轮询是常驻功耗：熄屏一整夜就是六千多次 HTTP。空闲退到 20 秒，
         * 只有真在干活（或刚失败、需要尽快发现恢复）时才回到 2 秒。
         */
        private const val IDLE_INTERVAL_MS = 20_000L
        /** 有回合在跑、或刚失败时用的间隔。 */
        private const val ACTIVE_INTERVAL_MS = 2_000L
        /** 连续这么多次轮询失败就认为服务器已死，重起一份。 */
        private const val HEALTH_FAILURES_BEFORE_RESTART = 3

        /** The one URL this process serves; read by the Activity after binding. */
        @Volatile
        private var urlOrError: Pair<String?, String?> = null to null

        private const val ACTION_ATTACH_EXTERNAL = "com.atomcode.app.ATTACH_EXTERNAL"
        private const val ACTION_DETACH_EXTERNAL = "com.atomcode.app.DETACH_EXTERNAL"
        private const val EXTRA_EXTERNAL_PORT = "external_port"

        fun start(context: Context) {
            context.startForegroundService(Intent(context, AgentService::class.java))
        }

        /**
         * 保持前台服务活着，但只陪跑外部 daemon。
         *
         * 进程内 daemon 由调用方用 JNI 停掉；这样通知、唤醒锁与忙碌统计还在，
         * 又不会有两份运行时同时写一份会话日志。
         */
        fun attachExternal(context: Context, port: Int) {
            context.startForegroundService(
                Intent(context, AgentService::class.java)
                    .setAction(ACTION_ATTACH_EXTERNAL)
                    .putExtra(EXTRA_EXTERNAL_PORT, port),
            )
        }

        /** 切回本机：清掉外部模式并重起进程内 daemon。 */
        fun detachExternal(context: Context) {
            context.startForegroundService(
                Intent(context, AgentService::class.java).setAction(ACTION_DETACH_EXTERNAL),
            )
        }

        fun stop(context: Context) {
            // 立刻清掉缓存的 URL。stopService 是异步的：紧接着的 start() 若看到旧 URL
            // 就以为“已经在跑”而跳过 startAgent()，服务器这一轮再也起不来。
            urlOrError = null to null
            context.stopService(Intent(context, AgentService::class.java))
        }
    }
}