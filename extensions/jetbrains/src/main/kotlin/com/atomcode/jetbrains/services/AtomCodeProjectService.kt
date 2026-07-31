package com.atomcode.jetbrains.services

import com.atomcode.jetbrains.daemon.AtomCodeDaemonClient
import com.atomcode.jetbrains.daemon.ApprovalMode
import com.atomcode.jetbrains.daemon.AuthStatusResponse
import com.atomcode.jetbrains.daemon.ChatEvent
import com.atomcode.jetbrains.daemon.ChatRequest
import com.atomcode.jetbrains.daemon.ChatStreamListener
import com.atomcode.jetbrains.daemon.ConnectionErrorKind
import com.atomcode.jetbrains.daemon.ConnectionState
import com.atomcode.jetbrains.daemon.CreateProviderRequest
import com.atomcode.jetbrains.daemon.DaemonAuth
import com.atomcode.jetbrains.daemon.ImageInput
import com.atomcode.jetbrains.daemon.MessageInfo
import com.atomcode.jetbrains.daemon.ModelInfo
import com.atomcode.jetbrains.daemon.PatchProviderRequest
import com.atomcode.jetbrains.daemon.PatchThinkingRequest
import com.atomcode.jetbrains.daemon.ProviderInfo
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

private const val BACKGROUND_HEALTH_INITIAL_DELAY_SECONDS = 5L
private const val BACKGROUND_HEALTH_INTERVAL_SECONDS = 30L

private data class ProjectConnectionAttempt(
    val key: DaemonConnectionKey,
    val future: CompletableFuture<ConnectionState>,
)

internal fun shouldResetBackgroundConnection(
    connectedKey: DaemonConnectionKey?,
    currentKey: DaemonConnectionKey,
): Boolean = connectedKey != null && connectedKey != currentKey

internal fun providerSetupRequired(
    providers: List<ProviderInfo>,
    defaultProvider: String,
    auth: AuthStatusResponse?,
): Boolean {
    if (providers.isEmpty()) return true
    val selected = providers.firstOrNull { it.name == defaultProvider }
        ?: providers.firstOrNull { it.isDefault }
    val authUnavailable = auth?.loggedIn != true || auth.expired
    // Older daemons do not expose requires_login. Keep the previous
    // fail-closed behaviour until both sides speak the new protocol.
    return selected?.requiresLogin?.let { it && authUnavailable } ?: authUnavailable
}

internal class ApprovalModeRuntimeState(initialMode: ApprovalMode = ApprovalMode.Build) {
    @Volatile
    var confirmedMode: ApprovalMode = initialMode
        private set

    @Volatile
    var displayMode: ApprovalMode = initialMode
        private set

    @Volatile
    var pendingMode: ApprovalMode? = null
        private set

    @Synchronized
    fun beginSwitch(mode: ApprovalMode): Boolean {
        if (pendingMode != null || displayMode == mode) return false
        displayMode = mode
        pendingMode = mode
        return true
    }

    @Synchronized
    fun completeSwitch(requested: ApprovalMode, responseMode: String): ApprovalMode {
        if (pendingMode != requested) return displayMode
        val applied = parseApprovalMode(responseMode, confirmedMode)
        confirmedMode = applied
        displayMode = applied
        pendingMode = null
        return displayMode
    }

    @Synchronized
    fun failSwitch(requested: ApprovalMode): ApprovalMode {
        if (pendingMode != requested) return displayMode
        displayMode = confirmedMode
        pendingMode = null
        return displayMode
    }

    @Synchronized
    fun refreshFromDaemon(responseMode: String): ApprovalMode {
        if (pendingMode != null) return displayMode
        val applied = parseApprovalMode(responseMode, confirmedMode)
        confirmedMode = applied
        displayMode = applied
        return displayMode
    }

    private fun parseApprovalMode(wire: String, fallback: ApprovalMode): ApprovalMode =
        when (wire) {
            ApprovalMode.AcceptEdits.wire -> ApprovalMode.AcceptEdits
            ApprovalMode.Auto.wire -> ApprovalMode.Auto
            ApprovalMode.Plan.wire -> ApprovalMode.Plan
            ApprovalMode.Build.wire -> ApprovalMode.Build
            else -> fallback
        }
}

@Service(Service.Level.PROJECT)
class AtomCodeProjectService(private val project: Project) : Disposable {
    private val changes = PropertyChangeSupport(this)
    private val settingsService = AtomCodeSettingsState.getInstance()
    private val auth = DaemonAuth(AtomCodeTokenFactory.createToken())
    private val daemonSupervisor = AtomCodeDaemonSupervisor.getInstance()

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

    private val approvalModeState = ApprovalModeRuntimeState()

    val approvalMode: ApprovalMode
        get() = approvalModeState.displayMode

    val confirmedApprovalMode: ApprovalMode
        get() = approvalModeState.confirmedMode

    val approvalModePending: Boolean
        get() = approvalModeState.pendingMode != null

    val fileChangeService = FileChangeService(project)

    @Volatile
    private var activeClient: AtomCodeDaemonClient? = null

    @Volatile
    private var activeClientKey: DaemonConnectionKey? = null

    @Volatile
    private var connectedEndpointKey: DaemonConnectionKey? = null

    private val backgroundHealthStarted = AtomicBoolean(false)
    private val backgroundHealthInFlight = AtomicBoolean(false)

    @Volatile
    private var backgroundHealthTask: ScheduledFuture<*>? = null

    private val ensureConnectedInFlight = AtomicReference<ProjectConnectionAttempt>()

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
        val settings = settingsService.state.copy()
        val key = DaemonConnectionKey.from(settings)
        val current = connectionState
        if (current is ConnectionState.Ready && connectedEndpointKey == key) {
            return CompletableFuture.completedFuture(current)
        }

        val existing = ensureConnectedInFlight.get()
        if (existing != null) {
            return if (existing.key == key) {
                existing.future
            } else {
                existing.future.thenCompose { ensureConnected() }
            }
        }

        val future = CompletableFuture<ConnectionState>()
        val attempt = ProjectConnectionAttempt(key, future)
        if (!ensureConnectedInFlight.compareAndSet(null, attempt)) {
            return ensureConnected()
        }

        ensureConnectedImpl(settings, key).whenComplete { result, error ->
            ensureConnectedInFlight.compareAndSet(attempt, null)
            future.complete(if (error != null) connectionState else result)
        }
        return future
    }

    private fun ensureConnectedImpl(
        settings: AtomCodeSettings,
        key: DaemonConnectionKey,
    ): CompletableFuture<ConnectionState> {
        setConnectionState(ConnectionState.CheckingDaemon)
        return daemonSupervisor.ensureReady(settings, auth)
            .thenCompose { ready ->
                setConnectionState(ConnectionState.Connecting)
                val client = newClient(settings)
                syncProjectDirectory(client, ready.version, key)
            }
            .exceptionally { error ->
                val cause = unwrapConnectionError(error)
                val errorState = when (cause) {
                    is DaemonConnectionException -> {
                        if (cause.kind == ConnectionErrorKind.MissingBinary) {
                            ConnectionState.SetupRequired(cause.message ?: "AtomCode daemon was not found.")
                        } else {
                            ConnectionState.Error(cause.kind, cause.message ?: "Connection failed")
                        }
                    }
                    else -> ConnectionState.Error(
                        ConnectionErrorKind.Unknown,
                        cause.message ?: "Connection failed",
                    )
                }
                clearActiveConnection()
                setConnectionState(errorState)
                errorState
            }
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

    fun setApprovalMode(mode: ApprovalMode): CompletableFuture<ApprovalMode> {
        if (!approvalModeState.beginSwitch(mode)) {
            return CompletableFuture.completedFuture(approvalModeState.displayMode)
        }
        val client = activeClient
        if (client == null) {
            return CompletableFuture.completedFuture(approvalModeState.completeSwitch(mode, mode.wire))
        }
        return client.setApprovalMode(mode).handle { response, error ->
            if (error == null) {
                approvalModeState.completeSwitch(mode, response.mode)
            } else {
                approvalModeState.failSwitch(mode)
            }
            approvalModeState.displayMode
        }
    }

    fun sendPrompt(
        prompt: String,
        session: SessionRefView?,
        listener: ChatStreamListener,
        provider: String? = null,
        images: List<ImageInput> = emptyList(),
        approvalMode: ApprovalMode? = null,
        onSessionReady: (SessionRefView) -> Unit,
    ): CompletableFuture<SessionRefView> {
        return saveDocumentsBeforePrompt().thenCompose {
            ensureConnected()
        }.thenCompose { state ->
            if (state !is ConnectionState.Ready) {
                CompletableFuture.failedFuture(IllegalStateException("AtomCode is not connected."))
            } else {
                sendPromptWhenReady(prompt, state.projectPath, session, listener, onSessionReady, provider, images, approvalMode)
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

    fun loadSetupSnapshot(): CompletableFuture<SetupSnapshot> {
        val settings = settingsService.state.copy()
        val key = DaemonConnectionKey.from(settings)
        val client = activeClient
        return if (connectionState is ConnectionState.Ready && connectedEndpointKey == key && client != null) {
            loadSetupSnapshot(client)
        } else {
            CompletableFuture.completedFuture(emptySetupSnapshot())
        }
    }

    private fun loadSetupSnapshot(client: AtomCodeDaemonClient): CompletableFuture<SetupSnapshot> {
        val authFuture = client.authStatus().exceptionally { null }
        val providersFuture = client.listProviders().exceptionally { null }
        val modelsFuture = client.listModels().exceptionally { emptyList() }

        return CompletableFuture.allOf(authFuture, providersFuture, modelsFuture).thenApply {
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
                setupRequired = providerSetupRequired(
                    providers = providers?.providers.orEmpty(),
                    defaultProvider = defaultProvider,
                    auth = auth,
                ),
            )
        }
    }

    fun loginWithBrowser(onStatus: (String) -> Unit): CompletableFuture<SetupSnapshot> {
        val settings = settingsService.state.copy()
        return daemonSupervisor.ensureReady(settings, auth).thenCompose {
            val client = newClient(settings)
            AtomCodeLoginCoordinator.getInstance().login(client, onStatus).thenCompose {
                loadSetupSnapshot(client)
            }.whenComplete { _, error ->
                if (error == null) ensureConnected()
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

        val settings = settingsService.state.copy()
        val key = DaemonConnectionKey.from(settings)
        if (shouldResetBackgroundConnection(connectedEndpointKey, key)) {
            clearActiveConnection()
            setConnectionState(ConnectionState.Idle)
            if (settings.autoStart) {
                backgroundHealthInFlight.set(false)
                ensureConnected()
                return
            }
        }

        val client = getOrCreateClient()
        client.health()
            .thenCompose { health ->
                if (health.service != "atomcode-daemon") {
                    CompletableFuture.failedFuture(IllegalStateException("Unexpected service on AtomCode port."))
                } else if (connectionState is ConnectionState.Ready) {
                    CompletableFuture.completedFuture(connectionState)
                } else {
                    syncProjectDirectory(client, health.version, key)
                }
            }
            .whenComplete { _, error ->
                backgroundHealthInFlight.set(false)
                if (error != null && !connectionState.isConnecting()) {
                    clearActiveConnection()
                    setConnectionState(ConnectionState.SetupRequired("AtomCode daemon is not running."))
                    if (settings.autoStart) ensureConnected()
                }
            }
    }

    private fun syncProjectDirectory(
        client: AtomCodeDaemonClient,
        version: String,
        key: DaemonConnectionKey,
    ): CompletableFuture<ConnectionState> {
        val basePath = project.basePath
        if (basePath.isNullOrBlank()) {
            activateClient(client, key)
            return refreshApprovalMode(client).thenApply {
                setConnectionState(ConnectionState.Ready(version, ""))
                connectionState
            }
        }

        setConnectionState(ConnectionState.SyncingProject)
        return client.changeDir(basePath)
            .thenApply { response ->
                if (!response.success) {
                    throw IllegalStateException("AtomCode daemon rejected project directory: ${response.message}")
                }
                activateClient(client, key)
                setConnectionState(ConnectionState.CheckingProvider)
                response.currentDir
            }
            .thenCompose { currentDir ->
                refreshApprovalMode(client).thenApply {
                    setConnectionState(ConnectionState.Ready(version, currentDir))
                    connectionState
                }
            }
    }

    private fun refreshApprovalMode(client: AtomCodeDaemonClient): CompletableFuture<Unit> {
        return client.getApprovalMode().handle { response, _ ->
            if (response == null) return@handle Unit
            approvalModeState.refreshFromDaemon(response.mode)
            Unit
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
        approvalMode: ApprovalMode?,
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
                approvalMode = (approvalMode ?: this.confirmedApprovalMode).wire,
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
        val settings = settingsService.state.copy()
        val key = DaemonConnectionKey.from(settings)
        activeClient?.takeIf { activeClientKey == key }?.let { return it }
        synchronized(this) {
            activeClient?.takeIf { activeClientKey == key }?.let { return it }
            val client = newClient(settings)
            activeClient = client
            activeClientKey = key
            return client
        }
    }

    private fun activateClient(client: AtomCodeDaemonClient, key: DaemonConnectionKey) {
        synchronized(this) {
            activeClient = client
            activeClientKey = key
            connectedEndpointKey = key
        }
    }

    private fun clearActiveConnection() {
        synchronized(this) {
            activeClient = null
            activeClientKey = null
            connectedEndpointKey = null
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
        clearActiveConnection()
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

private fun emptySetupSnapshot(): SetupSnapshot = SetupSnapshot(
    auth = null,
    providers = emptyList(),
    models = emptyList(),
    defaultProvider = "",
    currentModel = "",
    setupRequired = true,
)

private fun unwrapConnectionError(error: Throwable): Throwable {
    var current = error
    while (
        (current is java.util.concurrent.CompletionException ||
            current is java.util.concurrent.ExecutionException) && current.cause != null
    ) {
        current = requireNotNull(current.cause)
    }
    return current
}

data class SessionRefView(
    val id: String,
    val name: String,
    val projectHash: String,
    val workingDir: String,
)
