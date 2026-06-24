package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.AtomCodeDaemonClient
import com.atomcode.jetbrains.daemon.AtomCodeDaemonProcess
import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.ChatRequest
import com.atomcode.jetbrains.daemon.ChatStreamListener
import com.atomcode.jetbrains.daemon.ConnectionErrorKind
import com.atomcode.jetbrains.daemon.ConnectionState
import com.atomcode.jetbrains.daemon.CreateProviderRequest
import com.atomcode.jetbrains.daemon.DaemonAuth
import com.atomcode.jetbrains.daemon.HealthResponse
import com.atomcode.jetbrains.daemon.ImageInput
import com.atomcode.jetbrains.daemon.MessageInfo
import com.atomcode.jetbrains.daemon.ModelInfo
import com.atomcode.jetbrains.daemon.PatchProviderRequest
import com.atomcode.jetbrains.daemon.PatchThinkingRequest
import com.atomcode.jetbrains.daemon.SessionDetail
import com.atomcode.jetbrains.daemon.SessionMeta
import com.atomcode.jetbrains.daemon.SetupSnapshot
import com.atomcode.jetbrains.files.FileChangeService
import com.atomcode.jetbrains.security.AtomCodeTokenFactory
import com.atomcode.jetbrains.settings.AtomCodeSettings
import com.atomcode.jetbrains.settings.AtomCodeSettingsState
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.components.Service
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.project.Project
import com.intellij.util.concurrency.AppExecutorUtil
import java.beans.PropertyChangeListener
import java.beans.PropertyChangeSupport
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

private const val DAEMON_PROBE_TIMEOUT_MS = 3_000
private const val DAEMON_STARTUP_PROBE_TIMEOUT_MS = 1_000
private const val DAEMON_STARTUP_WAIT_SECONDS = 15L
private const val DAEMON_STARTUP_RETRY_DELAY_MS = 150L
private const val BACKGROUND_HEALTH_INITIAL_DELAY_SECONDS = 5L
private const val BACKGROUND_HEALTH_INTERVAL_SECONDS = 30L

@Service(Service.Level.PROJECT)
class AtomCodeProjectService(private val project: Project) : Disposable {
    private val changes = PropertyChangeSupport(this)
    private val settingsService = AtomCodeSettingsState.getInstance()
    private val auth = DaemonAuth(AtomCodeTokenFactory.createToken())

    @Volatile
    var connectionState: ConnectionState = ConnectionState.Idle
        private set

    @Volatile
    var activeSessionId: String? = null
        private set

    @Volatile
    private var activeProjectHash: String? = null

    @Volatile
    private var activeSessionWorkingDir: String? = null

    val fileChangeService = FileChangeService(project)

    @Volatile
    private var activeClient: AtomCodeDaemonClient? = null

    private val backgroundHealthStarted = AtomicBoolean(false)
    private val backgroundHealthInFlight = AtomicBoolean(false)

    @Volatile
    private var backgroundHealthTask: ScheduledFuture<*>? = null

    private val ensureConnectedInFlight = AtomicReference<CompletableFuture<ConnectionState>>()

    fun addConnectionListener(listener: PropertyChangeListener) {
        changes.addPropertyChangeListener("connectionState", listener)
    }

    fun removeConnectionListener(listener: PropertyChangeListener) {
        changes.removePropertyChangeListener("connectionState", listener)
    }

    fun startBackgroundHealthChecks() {
        if (!backgroundHealthStarted.compareAndSet(false, true)) return

        if (settingsService.state.autoStart) {
            ensureConnected()
        }

        backgroundHealthTask = AppExecutorUtil.getAppScheduledExecutorService().scheduleWithFixedDelay(
            {
                refreshConnectionHealth()
            },
            BACKGROUND_HEALTH_INITIAL_DELAY_SECONDS,
            BACKGROUND_HEALTH_INTERVAL_SECONDS,
            TimeUnit.SECONDS,
        )
    }

    fun ensureConnected(): CompletableFuture<ConnectionState> {
        // 已连接则短路，避免多余 HTTP 往返
        val current = connectionState
        if (current is ConnectionState.Ready) {
            return CompletableFuture.completedFuture(current)
        }

        val existing = ensureConnectedInFlight.get()
        if (existing != null) return existing

        val future = CompletableFuture<ConnectionState>()
        if (!ensureConnectedInFlight.compareAndSet(null, future)) {
            return ensureConnectedInFlight.get()!!
        }

        return ensureConnectedImpl()
            .whenComplete { result, error ->
                future.complete(if (error != null) connectionState else result)
                // 无论正常/异常/取消都清空缓存（exception + cancel）
            }
            .thenCompose { future }
            .whenComplete { _, _ -> ensureConnectedInFlight.set(null) }
    }

    private fun ensureConnectedImpl(): CompletableFuture<ConnectionState> {
        setConnectionState(ConnectionState.CheckingDaemon)
        val settings = settingsService.state.copy()
        val daemonProcess = AtomCodeDaemonProcess(settings)

        // 初始探测用短超时（3s），daemon 本地响应只需 <100ms
        // 避免 daemon 未运行时等满 30s 才超时
        val probeClient = AtomCodeDaemonClient(settings.host, settings.port, DAEMON_PROBE_TIMEOUT_MS, auth)
        val startupProbeClient = AtomCodeDaemonClient(settings.host, settings.port, DAEMON_STARTUP_PROBE_TIMEOUT_MS, auth)
        val client = AtomCodeDaemonClient(settings.host, settings.port, settings.requestTimeoutMs, auth)

        return probeClient.health()
            .handle { health, healthError ->
                if (healthError == null && health.service == "atomcode-daemon") {
                    setConnectionState(ConnectionState.Connecting)
                    val expectedVersion = daemonProcess.expectedBundledVersion()
                    val expectedHash = daemonProcess.expectedBundledHash()
                    val binaryMismatch = expectedHash != null && health.binaryHash != expectedHash
                    if (binaryMismatch || (expectedVersion != null && health.version != expectedVersion)) {
                        setConnectionState(ConnectionState.StartingDaemon)
                        return@handle restartMismatchedDaemon(
                            client,
                            daemonProcess,
                            health.version,
                            expectedVersion ?: health.version,
                        )
                    }
                    return@handle CompletableFuture.completedFuture(health.version)
                }

                if (!settings.autoStart) {
                    setConnectionState(ConnectionState.SetupRequired("AtomCode daemon is not running."))
                    return@handle CompletableFuture.completedFuture<String?>(null)
                }

                setConnectionState(ConnectionState.StartingDaemon)
                daemonProcess.ensureRunning(auth).thenCompose { started ->
                    if (!started) {
                        setConnectionState(ConnectionState.SetupRequired("AtomCode CLI was not found."))
                        CompletableFuture.completedFuture<String?>(null)
                    } else {
                        waitForDaemonReady(startupProbeClient)
                    }
                }
            }
            .thenCompose { it }
            .thenCompose { version ->
                if (version == null) {
                    CompletableFuture.completedFuture(connectionState)
                } else {
                    syncProjectDirectory(client, version)
                }
            }
            .exceptionally { error ->
                val errorState = ConnectionState.Error(ConnectionErrorKind.Unknown, error.message ?: "Connection failed")
                setConnectionState(errorState)
                errorState
            }
    }

    private fun restartMismatchedDaemon(
        client: AtomCodeDaemonClient,
        daemonProcess: AtomCodeDaemonProcess,
        runningVersion: String,
        expectedVersion: String,
    ): CompletableFuture<String?> {
        return client.shutdown()
            .exceptionally { false }
            .thenCompose { waitForDaemonStop(client, System.nanoTime() + TimeUnit.SECONDS.toNanos(5)) }
            .thenCompose { stopped ->
                if (!stopped) {
                    val message = "AtomCode daemon version mismatch: running $runningVersion, expected $expectedVersion. Stop the old daemon or change the daemon binary path."
                    setConnectionState(ConnectionState.Error(ConnectionErrorKind.IncompatibleDaemon, message))
                    CompletableFuture.completedFuture<String?>(null)
                } else {
                    daemonProcess.ensureRunning(auth).thenCompose { started ->
                        if (!started) {
                            setConnectionState(ConnectionState.SetupRequired("AtomCode CLI was not found."))
                            CompletableFuture.completedFuture<String?>(null)
                        } else {
                            waitForDaemonReady(client).thenApply { version ->
                                val healthVersion = version ?: throw IllegalStateException("AtomCode daemon did not become ready after restart.")
                                if (healthVersion != expectedVersion) {
                                    throw IllegalStateException("AtomCode daemon restarted with $healthVersion, expected $expectedVersion")
                                }
                                healthVersion
                            }
                        }
                    }
                }
            }
    }

    private fun waitForDaemonStop(client: AtomCodeDaemonClient, deadlineNanos: Long): CompletableFuture<Boolean> {
        return client.health()
            .handle { _, error -> error != null }
            .thenCompose { stopped ->
                if (stopped) {
                    CompletableFuture.completedFuture(true)
                } else if (System.nanoTime() >= deadlineNanos) {
                    CompletableFuture.completedFuture(false)
                } else {
                    CompletableFuture.supplyAsync(
                        { Unit },
                        CompletableFuture.delayedExecutor(100, TimeUnit.MILLISECONDS),
                    ).thenCompose { waitForDaemonStop(client, deadlineNanos) }
                }
            }
    }

    private fun waitForDaemonReady(client: AtomCodeDaemonClient): CompletableFuture<String?> {
        val deadlineNanos = System.nanoTime() + TimeUnit.SECONDS.toNanos(DAEMON_STARTUP_WAIT_SECONDS)
        return waitForDaemonHealth(deadlineNanos) { client.health() }
    }

    fun sendPrompt(prompt: String): CompletableFuture<Unit> {
        return sendPrompt(prompt, object : ChatStreamListener {})
    }

    fun sendPrompt(prompt: String, listener: ChatStreamListener): CompletableFuture<Unit> {
        return sendPrompt(prompt, currentSessionRef(), listener) {
            activeSessionId = it.id
            activeProjectHash = it.projectHash
            activeSessionWorkingDir = it.workingDir
        }.thenApply { Unit }
    }

    fun sendPrompt(
        prompt: String,
        session: SessionRefView?,
        listener: ChatStreamListener,
        provider: String? = null,
        images: List<ImageInput> = emptyList(),
        onSessionReady: (SessionRefView) -> Unit,
    ): CompletableFuture<SessionRefView> {
        return saveDocumentsBeforePrompt().thenCompose {
            ensureConnected()
        }.thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.failedFuture(IllegalStateException("AtomCode is not connected."))
            } else {
                sendPromptWhenReady(prompt, state.projectPath, session, listener, onSessionReady, provider, images)
            }
        }.whenComplete { _, error ->
            if (error != null) {
                val message = error.cause?.message ?: error.message ?: "Chat failed"
                listener.onError(message)
            }
        }
    }

    /**
     * Document saves mutate the IDE model and must originate from an IntelliJ
     * write-safe event, not an arbitrary Swing callback (such as queue handoff).
     */
    private fun saveDocumentsBeforePrompt(): CompletableFuture<Unit> {
        if (!settingsService.state.autoSaveBeforeRead) {
            return CompletableFuture.completedFuture(Unit)
        }

        val result = CompletableFuture<Unit>()
        ApplicationManager.getApplication().invokeLater({
            if (project.isDisposed) {
                result.completeExceptionally(IllegalStateException("Project is already disposed."))
                return@invokeLater
            }
            runCatching {
                FileDocumentManager.getInstance().saveAllDocuments()
            }.onSuccess {
                result.complete(Unit)
            }.onFailure(result::completeExceptionally)
        }, ModalityState.nonModal())
        return result
    }

    fun stopGeneration(): CompletableFuture<Unit> {
        return stopGeneration(activeSessionId)
    }

    fun stopGeneration(sessionId: String?): CompletableFuture<Unit> {
        val sessionId = sessionId ?: return CompletableFuture.completedFuture(Unit)
        val client = getOrCreateClient()
        return client.stopChat(sessionId).thenApply { Unit }
    }

    fun respondToPermission(
        sessionId: String,
        decision: String,
        toolName: String? = null,
    ): CompletableFuture<Boolean> {
        val client = getOrCreateClient()
        return client.sendPermissionDecision(sessionId, decision, toolName).thenApply {
            if (!it.success && !it.error.isNullOrBlank()) {
                throw IllegalStateException(it.error)
            }
            it.success
        }
    }

    fun refreshSessions(): CompletableFuture<List<SessionMeta>> =
        ensureConnected().thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.completedFuture(emptyList())
            } else {
                val client = getOrCreateClient()
                client.listSessions()
            }
        }

    fun searchSessions(query: String): CompletableFuture<List<SessionMeta>> =
        ensureConnected().thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.completedFuture(emptyList())
            } else {
                val client = getOrCreateClient()
                client.searchSessions(query)
            }
        }

    fun loadSession(meta: SessionMeta): CompletableFuture<SessionDetail> {
        return loadSessionDetail(meta).thenApply {
            activeSessionId = it.id
            activeProjectHash = it.projectHash
            activeSessionWorkingDir = it.workingDir
            it
        }
    }

    fun loadSessionDetail(meta: SessionMeta): CompletableFuture<SessionDetail> {
        val client = getOrCreateClient()
        return client.getSession(meta.projectHash, meta.id)
    }

    fun renameSession(meta: SessionMeta, name: String): CompletableFuture<List<SessionMeta>> {
        val client = getOrCreateClient()
        return client.renameSession(meta.projectHash, meta.id, name).thenCompose {
            client.listSessions()
        }
    }

    fun deleteSession(meta: SessionMeta): CompletableFuture<List<SessionMeta>> {
        val client = getOrCreateClient()
        return client.deleteSession(meta.projectHash, meta.id).thenCompose {
            if (activeSessionId == meta.id) {
                activeSessionId = null
                activeProjectHash = null
                activeSessionWorkingDir = null
            }
            client.listSessions()
        }
    }

    fun deleteSessions(metas: List<SessionMeta>): CompletableFuture<List<SessionMeta>> {
        if (metas.isEmpty()) {
            return refreshSessions()
        }
        val client = getOrCreateClient()
        val chain = metas.fold(CompletableFuture.completedFuture(Unit)) { future, meta ->
            future.thenCompose {
                client.deleteSession(meta.projectHash, meta.id).thenApply {
                    if (activeSessionId == meta.id) {
                        activeSessionId = null
                        activeProjectHash = null
                        activeSessionWorkingDir = null
                    }
                    Unit
                }
            }
        }
        return chain.thenCompose { client.listSessions() }
    }

    fun startNewSession(): CompletableFuture<SessionRefView> =
        createSession().thenApply {
            activeSessionId = it.id
            activeProjectHash = it.projectHash
            activeSessionWorkingDir = it.workingDir
            it
        }

    fun createSession(): CompletableFuture<SessionRefView> =
        ensureConnected().thenCompose { state ->
            val path = when (state) {
                is ConnectionState.Ready -> state.projectPath.ifBlank { project.basePath.orEmpty() }
                else -> project.basePath.orEmpty()
            }
            val client = getOrCreateClient()
            client.createSession("AtomCode Chat", path).thenApply {
                SessionRefView(it.id, it.name, it.projectHash, it.workingDir)
            }
        }

    fun loadSetupSnapshot(): CompletableFuture<SetupSnapshot> =
        ensureConnected().thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.completedFuture(
                    SetupSnapshot(
                        auth = null,
                        providers = emptyList(),
                        models = emptyList(),
                        defaultProvider = "",
                        currentModel = "",
                        setupRequired = true,
                    ),
                )
            } else {
                val client = getOrCreateClient()
                val authFuture = client.authStatus().exceptionally { null }
                val providersFuture = client.listProviders().exceptionally { null }
                val modelsFuture = client.listModels().exceptionally { emptyList() }

                CompletableFuture.allOf(authFuture, providersFuture, modelsFuture).thenApply {
                    val auth = authFuture.get()
                    val providers = providersFuture.get()
                    val models = modelsFuture.get()
                    val defaultProvider = providers?.defaultProvider.orEmpty()
                    val currentModel = providers?.providers?.firstOrNull { it.isDefault }?.model
                        ?: models.firstOrNull { it.isDefault }?.model
                        ?: ""
                    SetupSnapshot(
                        auth = auth,
                        providers = providers?.providers.orEmpty(),
                        models = models,
                        defaultProvider = defaultProvider,
                        currentModel = currentModel,
                        setupRequired = auth?.loggedIn != true || providers?.providers.isNullOrEmpty(),
                    )
                }
            }
        }

    fun loginWithBrowser(onStatus: (String) -> Unit): CompletableFuture<SetupSnapshot> =
        ensureConnected().thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.failedFuture(IllegalStateException("AtomCode is not connected."))
            } else {
                val client = getOrCreateClient()
                client.startLogin(true).thenCompose { start ->
                    onStatus("Opened browser for AtomGit sign-in.")
                    pollLoginUntilAuthorized(client, start.loginId, start.expiresInSeconds, onStatus)
                }.thenCompose {
                    loadSetupSnapshot()
                }
            }
        }

    fun setDefaultModel(model: ModelInfo): CompletableFuture<SetupSnapshot> {
        val client = getOrCreateClient()
        return client.setDefaultProvider(model.provider).thenCompose {
            loadSetupSnapshot()
        }
    }

    fun createProvider(request: CreateProviderRequest): CompletableFuture<SetupSnapshot> {
        val client = getOrCreateClient()
        return client.createProvider(request).thenCompose {
            loadSetupSnapshot()
        }
    }

    fun patchProvider(request: PatchProviderRequest): CompletableFuture<SetupSnapshot> {
        val client = getOrCreateClient()
        return client.patchProvider(request).thenCompose {
            loadSetupSnapshot()
        }
    }

    fun deleteProvider(name: String): CompletableFuture<SetupSnapshot> {
        val client = getOrCreateClient()
        return client.deleteProvider(name).thenCompose {
            loadSetupSnapshot()
        }
    }

    fun patchProviderThinking(name: String, request: PatchThinkingRequest): CompletableFuture<SetupSnapshot> {
        val client = getOrCreateClient()
        return client.patchThinking(name, request).thenCompose {
            loadSetupSnapshot()
        }
    }

    fun setupCodingPlan(): CompletableFuture<String> {
        val client = getOrCreateClient()
        return client.setupCodingPlan().thenCompose { response ->
            loadSetupSnapshot().thenApply {
                response.reportText.ifBlank {
                    if (response.success) {
                        "CodingPlan setup completed. Default provider: ${response.defaultProvider}"
                    } else {
                        "CodingPlan setup did not complete."
                    }
                }
            }
        }
    }

    private fun refreshConnectionHealth() {
        if (project.isDisposed) return
        if (connectionState.isConnecting()) return
        if (!backgroundHealthInFlight.compareAndSet(false, true)) return

        val client = getOrCreateClient()
        client.health()
            .thenCompose { health ->
                if (health.service != "atomcode-daemon") {
                    CompletableFuture.failedFuture(IllegalStateException("Unexpected service on AtomCode port."))
                } else if (connectionState is ConnectionState.Ready) {
                    CompletableFuture.completedFuture(connectionState)
                } else {
                    syncProjectDirectory(client, health.version)
                }
            }
            .whenComplete { _, error ->
                backgroundHealthInFlight.set(false)
                if (error != null && !connectionState.isConnecting()) {
                    activeClient = null
                    setConnectionState(ConnectionState.SetupRequired("AtomCode daemon is not running."))
                }
            }
    }

    private fun pollLoginUntilAuthorized(
        client: AtomCodeDaemonClient,
        loginId: String,
        expiresInSeconds: Int,
        onStatus: (String) -> Unit,
    ): CompletableFuture<Unit> {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(expiresInSeconds.coerceAtLeast(30).toLong())
        fun poll(): CompletableFuture<Unit> {
            return client.pollLogin(loginId).thenCompose { result ->
                when (result.status) {
                    "authorized" -> {
                        onStatus("Signed in${result.userName?.let { " as $it" } ?: ""}.")
                        CompletableFuture.completedFuture(Unit)
                    }
                    "pending" -> {
                        if (System.nanoTime() >= deadline) {
                            client.cancelLogin(loginId)
                            CompletableFuture.failedFuture(IllegalStateException("Login timed out."))
                        } else {
                            onStatus("Waiting for browser authorization...")
                            CompletableFuture.supplyAsync(
                                { Unit },
                                CompletableFuture.delayedExecutor(2, TimeUnit.SECONDS),
                            ).thenCompose { poll() }
                        }
                    }
                    else -> CompletableFuture.failedFuture(IllegalStateException("Unexpected login status: ${result.status}"))
                }
            }
        }
        return poll()
    }

    private fun syncProjectDirectory(client: AtomCodeDaemonClient, version: String): CompletableFuture<ConnectionState> {
        val basePath = project.basePath
        if (basePath.isNullOrBlank()) {
            activeClient = client
            setConnectionState(ConnectionState.Ready(version, ""))
            return CompletableFuture.completedFuture(connectionState)
        }

        setConnectionState(ConnectionState.SyncingProject)
        return client.changeDir(basePath)
            .thenApply { response ->
                if (!response.success) {
                    throw IllegalStateException("AtomCode daemon rejected project directory: ${response.message}")
                }
                activeClient = client
                setConnectionState(ConnectionState.CheckingProvider)
                setConnectionState(ConnectionState.Ready(version, response.currentDir))
                connectionState
            }
    }

    private fun sendPromptWhenReady(
        prompt: String,
        projectPath: String,
        session: SessionRefView?,
        listener: ChatStreamListener,
        onSessionReady: (SessionRefView) -> Unit,
        provider: String?,
        images: List<ImageInput>,
    ): CompletableFuture<SessionRefView> {
        val client = getOrCreateClient()
        val workingDir = projectPath.ifBlank { project.basePath.orEmpty() }
        val sessionFuture = session?.let { CompletableFuture.completedFuture(it) }
            ?: client.createSession("AtomCode Chat", workingDir).thenApply {
                SessionRefView(it.id, it.name, it.projectHash, it.workingDir)
            }

        return sessionFuture.thenCompose { sessionRef ->
            onSessionReady(sessionRef)
            val terminalEventSeen = AtomicBoolean(false)
            val request = ChatRequest(
                message = prompt,
                workingDir = sessionRef.workingDir.ifBlank { workingDir },
                sessionId = sessionRef.id,
                provider = provider,
                images = images,
            )

            client.streamChat(request) { event ->
                when (event) {
                    is ChatEvent.Done -> {
                        terminalEventSeen.set(true)
                    }
                    ChatEvent.Stopped,
                    is ChatEvent.Error -> terminalEventSeen.set(true)
                    else -> Unit
                }
                listener.onEvent(event)
            }.thenApply {
                if (!terminalEventSeen.get()) {
                    listener.onComplete()
                }
                sessionRef
            }
        }
    }

    private fun currentSessionRef(): SessionRefView? {
        val id = activeSessionId ?: return null
        return SessionRefView(
            id = id,
            name = "AtomCode Chat",
            projectHash = activeProjectHash.orEmpty(),
            workingDir = activeSessionWorkingDir.orEmpty(),
        )
    }

    private fun newClient(settings: AtomCodeSettings): AtomCodeDaemonClient =
        AtomCodeDaemonClient(settings.host, settings.port, settings.requestTimeoutMs, auth)

    private fun getOrCreateClient(): AtomCodeDaemonClient {
        activeClient?.let { return it }
        synchronized(this) {
            activeClient?.let { return it }
            val settings = settingsService.state.copy()
            val client = newClient(settings)
            activeClient = client
            return client
        }
    }

    private fun setConnectionState(next: ConnectionState) {
        val previous = connectionState
        connectionState = next
        ApplicationManager.getApplication().invokeLater {
            changes.firePropertyChange("connectionState", previous, next)
        }
    }

    override fun dispose() {
        backgroundHealthTask?.cancel(false)
        backgroundHealthTask = null
        changes.propertyChangeListeners.forEach {
            changes.removePropertyChangeListener(it)
        }
        activeClient = null
    }

    companion object {
        fun getInstance(project: Project): AtomCodeProjectService =
            project.getService(AtomCodeProjectService::class.java)
    }
}

private fun ConnectionState.isConnecting(): Boolean =
    this == ConnectionState.CheckingDaemon ||
        this == ConnectionState.StartingDaemon ||
        this == ConnectionState.Connecting ||
        this == ConnectionState.SyncingProject ||
        this == ConnectionState.CheckingProvider

internal fun waitForDaemonHealth(
    deadlineNanos: Long,
    retryDelayMs: Long = DAEMON_STARTUP_RETRY_DELAY_MS,
    health: () -> CompletableFuture<HealthResponse>,
): CompletableFuture<String?> =
    health()
        .handle { response, error ->
            if (error == null && response.service == "atomcode-daemon") response.version else null
        }
        .thenCompose { version ->
            if (version != null) {
                CompletableFuture.completedFuture(version)
            } else if (System.nanoTime() >= deadlineNanos) {
                CompletableFuture.completedFuture(null)
            } else {
                CompletableFuture.supplyAsync(
                    { Unit },
                    CompletableFuture.delayedExecutor(retryDelayMs, TimeUnit.MILLISECONDS),
                ).thenCompose { waitForDaemonHealth(deadlineNanos, retryDelayMs, health) }
            }
        }

data class SessionRefView(
    val id: String,
    val name: String,
    val projectHash: String,
    val workingDir: String,
)
