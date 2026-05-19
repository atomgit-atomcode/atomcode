import * as vscode from 'vscode';
import * as path from 'path';
import * as fs from 'fs';
import { DaemonClient } from '../daemon/client';
import {
  AuthStatusResponse,
  ChatRequest,
  CodingPlanSetupResponse,
  ConfigResponse,
  CreateProviderRequest,
  ModelInfo,
  MessageInfo,
  PatchThinkingRequest,
  ProvidersResponse,
} from '../daemon/types';

type WebviewMode = 'sidebar' | 'tab';
type QueuedChatMessage = { text: string; contextPaths?: string[]; clientMessageId?: string };
const PANEL_READY_TIMEOUT_MS = 5000;

interface SessionRuntime {
  abortController?: AbortController;
  isGenerating: boolean;
  queuedMessages: QueuedChatMessage[];
  projectHash?: string;
  errorMessage?: string;
  hasUnread?: boolean;
  eventBuffer: Array<{
    type: 'userMessage' | 'text' | 'toolBatchStart' | 'toolStart' | 'toolResult' | 'tokens';
    data: any;
  }>;
}

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _panel?: vscode.WebviewPanel;
  private _activeSessionId?: string;
  private _loadingSessionId?: string;
  private _loadedMessages?: MessageInfo[];
  private _sessionRuntimes = new Map<string, SessionRuntime>();
  private _loginId?: string;
  private _loginPoll?: ReturnType<typeof setInterval>;
  private _loginStartedFromCommand = false;
  private _panelReady = false;
  private _panelReadyPromise?: Promise<void>;
  private _panelReadyResolver?: () => void;

  public onModelSelected?: (model: string) => void;

  constructor(
    private readonly _extensionUri: vscode.Uri,
    private readonly _client: DaemonClient,
  ) {}

  public dispose() {
    this._clearLoginPoll();
  }

  public openInTab() {
    if (this._panel) {
      this._panel.reveal();
      return;
    }

    this._panelReady = false;
    this._panelReadyPromise = new Promise((resolve) => {
      this._panelReadyResolver = resolve;
    });

    this._panel = vscode.window.createWebviewPanel(
      'atomcode.chatTab',
      'AtomCode',
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [
          vscode.Uri.joinPath(this._extensionUri, 'webview'),
          vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
        ],
      },
    );

    this._panel.webview.html = this._getHtml(this._panel.webview, 'tab');
    this._setupWebviewMessageHandler(this._panel.webview, 'tab');

    this._panel.onDidDispose(() => {
      this._panel = undefined;
      this._panelReady = false;
      this._panelReadyResolver = undefined;
      this._panelReadyPromise = undefined;
    });
  }

  public async openInSidebar() {
    await vscode.commands.executeCommand('workbench.view.extension.atomcode');
    await vscode.commands.executeCommand('atomcode.chatView.focus');
  }

  public async openPreferredLocation() {
    const preferred = vscode.workspace.getConfiguration('atomcode').get<string>('preferredLocation', 'sidebar');
    if (preferred === 'panel') {
      this.openInTab();
    } else {
      await this.openInSidebar();
    }
  }

  public async openForEditorCommand() {
    this.openInTab();
    await this._waitForPanelReady(PANEL_READY_TIMEOUT_MS);
  }

  resolveWebviewView(webviewView: vscode.WebviewView) {
    this._view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this._extensionUri, 'webview'),
        vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
      ],
    };
    webviewView.webview.html = this._getHtml(webviewView.webview, 'sidebar');
    this._setupWebviewMessageHandler(webviewView.webview, 'sidebar');

    webviewView.onDidChangeVisibility(() => {
      vscode.commands.executeCommand('setContext', 'atomcode.chatFocused', webviewView.visible);
    });
  }

  private _setupWebviewMessageHandler(webview: vscode.Webview, mode: WebviewMode) {
    webview.onDidReceiveMessage(async (msg) => {
      switch (msg.type) {
        case 'send':
          await this._handleSend(
            msg.text,
            msg.context?.map((c: { path: string }) => c.path),
            msg.clientMessageId,
          );
          break;
        case 'stop':
          this.stopGeneration();
          break;
        case 'newConversation':
          await this.newConversation();
          break;
        case 'ready':
          this._markPanelReady(webview);
          await this._sendInitialState(webview, mode);
          break;
        case 'selectModel':
          await this._setDefaultProvider(msg.provider || msg.model);
          break;
        case 'authLoginStart':
          await this._startLogin();
          break;
        case 'authLoginCancel':
          await this._cancelLogin();
          break;
        case 'codingPlanSetup':
          await this._setupCodingPlan({ loginIfNeeded: true });
          break;
        case 'providerCreate':
          await this._createProvider(msg.provider);
          break;
        case 'providerDelete':
          await this._deleteProvider(msg.name);
          break;
        case 'providerSetDefault':
          await this._setDefaultProvider(msg.name);
          break;
        case 'providerPatchThinking':
          await this._patchThinking(msg.name, msg.thinking);
          break;
        case 'refreshSetupState':
          await this._sendSetupState(webview);
          break;
        case 'loadSession':
          await this._loadSession(msg.sessionId, msg.projectHash);
          break;
        case 'renameSession':
          await this._renameSession(msg.sessionId, msg.projectHash, msg.name);
          break;
        case 'deleteSession':
          await this._deleteSession(msg.sessionId, msg.projectHash, msg.name);
          break;
        case 'openSettings':
          vscode.commands.executeCommand('workbench.action.openSettings', 'atomcode');
          break;
        case 'openFile':
          if (msg.path) {
            const uri = vscode.Uri.file(msg.path);
            vscode.window.showTextDocument(uri);
          }
          break;
        case 'applyCode':
          await this._applyCode(msg.code, msg.language);
          break;
        case 'copyCode':
          vscode.env.clipboard.writeText(msg.code);
          break;
        case 'quickAction':
          await this._handleQuickAction(msg.action);
          break;
        case 'slashCommand':
          await this._handleSlashCommand(msg.command);
          break;
        case 'searchSessions':
          await this._searchSessions(msg.query);
          break;
        case 'popout':
          this.openInTab();
          break;
        case 'attachFile': {
          const uris = await vscode.window.showOpenDialog({
            canSelectFiles: true,
            canSelectMany: false,
            openLabel: 'Attach to AtomCode',
          });
          if (uris && uris.length > 0) {
            const filePath = uris[0].fsPath;
            const fileName = path.basename(filePath);
            this._postMessage({
              type: 'context',
              filePath,
              fileName,
              language: '',
            });
          }
          break;
        }
      }
    });
  }

  // Public API for commands
  public async sendMessage(text: string) {
    await this.openPreferredLocation();
    this._postMessage({ type: 'userMessage', text });
    await this._handleSend(text);
  }

  public async sendEditorCommandMessage(text: string) {
    await this.openForEditorCommand();
    if (this._activeSessionId) {
      const rt = this._getRuntime(this._activeSessionId);
      if (rt.isGenerating) {
        this.stopGeneration();
      }
    }
    await this._createEditorCommandSession();
    this._postMessage({ type: 'userMessage', text });
    await this._handleSend(text);
  }

  public async newConversation() {
    this.openInTab();

    // Keep the previous session's runtime intact — its stream (if any)
    // continues in the background with events buffered in its eventBuffer.
    this._activeSessionId = undefined;
    this._loadedMessages = undefined;

    try {
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const session = await this._client.createSession(undefined, workspaceFolder);
      this._activeSessionId = session.id;
      this._getRuntime(session.id).projectHash = session.project_hash;
      this._broadcastMessage({ type: 'sessionSelected', sessionId: session.id, projectHash: session.project_hash });
      await this._refreshSessions();
    } catch {
      this._broadcastMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
    }

    this._postMessage({ type: 'clearChat' });
    this.focusInput();
  }

  public stopGeneration() {
    if (!this._activeSessionId) return;
    const rt = this._sessionRuntimes.get(this._activeSessionId);
    if (!rt?.isGenerating) return;

    rt.abortController?.abort();
    rt.abortController = undefined;
    rt.queuedMessages = [];
    rt.isGenerating = false;
    rt.eventBuffer = [];
    void this._client.stopGeneration(this._activeSessionId).catch(() => undefined);
    this._postMessage({ type: 'generationStopped' });
  }

  public focusInput() {
    if (!this._panel) {
      this._view?.show(true);
    }
    this._postMessage({ type: 'focusInput' });
  }

  // Private
  private _getRuntime(sessionId: string): SessionRuntime {
    let rt = this._sessionRuntimes.get(sessionId);
    if (!rt) {
      rt = { isGenerating: false, queuedMessages: [], eventBuffer: [] };
      this._sessionRuntimes.set(sessionId, rt);
    }
    return rt;
  }

  private async _handleSend(text: string, contextPaths?: string[], clientMessageId?: string) {
    const trimmed = text.trim();
    if (!trimmed) return;

    // 在任何 await 之前捕获活跃 session ID——另一个 webview 处理器
    // （如侧边栏 newConversation）可能在微任务边界清除 _activeSessionId。
    let sid = this._activeSessionId;
    if (!sid) {
      await this._ensureSession();
      sid = this._activeSessionId;
    }
    if (!sid) return;
    const rt = this._getRuntime(sid);

    if (rt.isGenerating) {
      rt.queuedMessages.push({ text: trimmed, contextPaths, clientMessageId });
      return;
    }

    if (clientMessageId) {
      this._postMessage({ type: 'queuedMessageSent', id: clientMessageId });
    }

    if (await this._handleLocalCommand(trimmed)) {
      return;
    }

    rt.isGenerating = true;
    rt.eventBuffer = [];  // Start a fresh buffer for this turn
    rt.eventBuffer.push({ type: 'userMessage', data: { text: trimmed } });
    this._postMessage({ type: 'generationStarted' });

    let fullMessage = trimmed;
    if (contextPaths && contextPaths.length > 0) {
      const parts: string[] = [];
      const MAX_FILE_SIZE_BYTES = 512 * 1024; // 512 KB per file
      const MAX_TOTAL_BYTES = 1024 * 1024;    // 1 MB total across all files
      let totalBytes = 0;

      for (const filePath of contextPaths) {
        try {
          const uri = vscode.Uri.file(filePath);
          const content = await vscode.workspace.fs.readFile(uri);

          if (content.byteLength > MAX_FILE_SIZE_BYTES) {
            parts.push(`File: ${path.basename(filePath)}\n[File too large to attach (${Math.round(content.byteLength / 1024)} KB). Use a specific selection instead.]`);
            continue;
          }

          if (totalBytes + content.byteLength > MAX_TOTAL_BYTES) {
            parts.push(`File: ${path.basename(filePath)}\n[Skipped: total context size limit reached.]`);
            continue;
          }

          const decoded = Buffer.from(content).toString('utf-8');
          const fileName = path.basename(filePath);
          const ext = path.extname(filePath).slice(1);
          parts.push(`File: ${fileName}\n\`\`\`${ext}\n${decoded}\n\`\`\``);
          totalBytes += content.byteLength;
        } catch {
          // Skip files that can't be read
        }
      }
      if (parts.length > 0) {
        fullMessage = 'The user has attached the following file(s) for context. The content is provided inline below — DO NOT use read_file to re-read them.\n\n'
          + parts.join('\n\n') + '\n\n' + 'User question: ' + trimmed;
      }
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const request: ChatRequest = {
      message: fullMessage,
      working_dir: workspaceFolder,
      session_id: sid,
    };

    // Capture session ID so callbacks always reference the correct session
    const streamSessionId = sid;

    rt.abortController = this._client.streamChat(request, {
      onText: (content) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'text', data: { content } });
        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'text', content });
        }
      },
      onToolBatch: (calls) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolBatchStart' as const, data: { calls } });
        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'toolBatchStart', calls });
        }
      },
      onToolStart: (id, name, args) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolStart', data: { id, name, args } });
        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'toolStart', id, name, args });
        }
      },
      onToolResult: (id, name, output, success, durationMs) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolResult', data: { id, name, output, success, durationMs } });
        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'toolResult', id, name, output, success, durationMs });
        }
      },
      onTokens: (prompt, completion, total) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'tokens', data: { prompt, completion, total } });
        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'tokens', prompt, completion, total });
        }
      },
      onArtifactStart: (id, artifactType, language, title) =>
        this._postMessage({ type: 'artifactStart', id, artifactType, language, title }),
      onArtifactContent: (id, content) =>
        this._postMessage({ type: 'artifactContent', id, content }),
      onArtifactEnd: (id) =>
        this._postMessage({ type: 'artifactEnd', id }),
      onDone: (tokens, toolCalls, sessionId) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.isGenerating = false;
        srt.eventBuffer = [];

        // If the daemon assigned a different session ID (e.g. first turn
        // creates a permanent ID), migrate the runtime so the map stays
        // keyed correctly.
        if (sessionId && sessionId !== streamSessionId) {
          this._sessionRuntimes.set(sessionId, srt);
          this._sessionRuntimes.delete(streamSessionId);
        }

        if (this._activeSessionId === streamSessionId) {
          if (sessionId && sessionId !== streamSessionId) {
            this._activeSessionId = sessionId;
            this._loadedMessages = undefined;
            this._broadcastMessage({ type: 'sessionSelected', sessionId });
          }
          this._postMessage({ type: 'done', tokens, toolCalls, sessionId });
          void this._refreshSessions();
          setTimeout(() => void this._sendNextQueuedMessage(), 75);
        } else {
          // Stream completed in background — mark as unread so the session
          // list shows a solid green dot until the user switches back.
          srt.hasUnread = true;
          void this._refreshSessions();
          // Drain any queued messages via the (possibly migrated) session ID
          void this._sendNextQueuedMessageForSession(sessionId || streamSessionId);
        }
      },
      onStopped: () => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.isGenerating = false;
        srt.queuedMessages = [];
        srt.eventBuffer = [];

        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'stopped' });
        }
      },
      onError: (message) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.isGenerating = false;
        srt.queuedMessages = [];
        srt.eventBuffer = [];

        if (this._activeSessionId === streamSessionId) {
          this._postMessage({ type: 'error', message });
        } else {
          // Store the error so it can be surfaced when the user switches
          // back to this session (see _loadSession).
          srt.errorMessage = message;
        }
      },
    });
  }

  private async _sendNextQueuedMessage() {
    const sid = this._activeSessionId;
    if (!sid) return;
    const rt = this._sessionRuntimes.get(sid);
    if (!rt || rt.isGenerating) return;

    const next = rt.queuedMessages.shift();
    if (!next) return;

    await this._handleSend(next.text, next.contextPaths, next.clientMessageId);

    const rt2 = this._sessionRuntimes.get(sid);
    if (rt2 && !rt2.isGenerating) {
      void this._sendNextQueuedMessage();
    }
  }

  private async _sendNextQueuedMessageForSession(sessionId: string) {
    // Only drain queued messages for the active session. Background sessions
    // keep their queued messages until the user switches back, at which point
    // _loadSession drains them.
    if (this._activeSessionId !== sessionId) return;
    await this._sendNextQueuedMessage();
  }

  private async _ensureSession() {
    if (this._activeSessionId) return;

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const session = await this._client.createSession(undefined, workspaceFolder);
    this._activeSessionId = session.id;
    this._getRuntime(session.id).projectHash = session.project_hash;
    this._loadedMessages = undefined;
    this._broadcastMessage({ type: 'sessionSelected', sessionId: session.id, projectHash: session.project_hash });
    await this._refreshSessions();
  }

  private async _createEditorCommandSession() {
    // Keep the previous session's runtime intact — sendEditorCommandMessage
    // already calls stopGeneration() when needed, which properly stops the
    // daemon stream and marks isGenerating=false.
    this._activeSessionId = undefined;
    this._loadedMessages = undefined;
    this._postMessage({ type: 'clearChat' });
    await this._ensureSession();
  }

  public sendEditorContext() {
    this._sendEditorContext();
  }

  // New protocol methods

  private async _sendInitialState(webview?: vscode.Webview, mode: WebviewMode = 'tab') {
    let currentModelName = '';

    await this._sendSetupState(webview);

    // Send models
    try {
      const models = await this._client.listModels();
      this._postMessage({ type: 'models', models }, webview);
      const defaultModel = models.find((m: { is_default: boolean }) => m.is_default);
      if (defaultModel) {
        currentModelName = (defaultModel as { model: string }).model || '';
      }
    } catch {
      // daemon not available
    }

    // Send sessions
    try {
      const sessions = await this._client.listSessions();
      await this._annotateSessionGenerating(sessions as any[]);
      this._postMessage({ type: 'sessions', sessions }, webview);
    } catch {}

    // Send editor context
    this._sendEditorContext(webview);

    // Send session messages FIRST — LOAD_SESSION_MESSAGES sets isGenerating=false
    // in the reducer, which is then corrected by _replayStreamBuffer below.
    if (this._loadedMessages && mode === 'tab') {
      this._postMessage({ type: 'sessionMessages', messages: this._loadedMessages }, webview);
    }

    // If the active session has a running background stream, replay buffered
    // events so the webview shows the streaming assistant and live content.
    if (this._activeSessionId) {
      const rt = this._sessionRuntimes.get(this._activeSessionId);
      if (rt) {
        this._replayStreamBuffer(this._activeSessionId, rt, webview);

        if (rt.errorMessage) {
          this._postMessage({ type: 'error', message: rt.errorMessage }, webview);
          rt.errorMessage = undefined;
        }
      }
    }

    // Send init LAST — it confirms the state already established above
    this._postMessage({
      type: 'init',
      generating: this._activeSessionId
        ? (this._sessionRuntimes.get(this._activeSessionId)?.isGenerating ?? false)
        : false,
      currentModel: currentModelName,
      viewMode: mode,
      activeSessionId: this._activeSessionId,
    }, webview);
  }

  private async _sendSetupState(webview?: vscode.Webview) {
    let auth: AuthStatusResponse | undefined;
    let providers: ProvidersResponse | undefined;
    let config: ConfigResponse | undefined;
    let models: ModelInfo[] | undefined;
    const post = (msg: unknown) => webview
      ? this._postMessage(msg, webview)
      : this._broadcastMessage(msg);

    try {
      auth = await this._client.authStatus();
      post({ type: 'authStatus', auth });
    } catch (e) {
      post({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      providers = await this._client.listProviders();
      post({ type: 'providers', providers: providers.providers, defaultProvider: providers.default_provider });
    } catch (e) {
      post({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      config = await this._client.getConfig();
      post({ type: 'config', config });
    } catch {
      // Older daemons may not have P0 APIs; provider fetch error already surfaces enough.
    }

    try {
      models = await this._client.listModels();
      post({ type: 'models', models });
    } catch {}

    const defaultProvider = providers?.providers.find((p) => p.is_default);
    post({
      type: 'setupState',
      auth,
      providers: providers?.providers ?? [],
      defaultProvider: providers?.default_provider ?? config?.default_provider ?? '',
      currentModel: defaultProvider?.model || models?.find((m) => m.is_default)?.model || '',
      setupRequired: !auth?.logged_in || (providers?.providers.length ?? 0) === 0,
    });
  }

  private async _startLogin() {
    try {
      await this._cancelLogin();
      const login = await this._client.startLogin(true);
      this._loginId = login.login_id;
      this._postMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });

      this._loginPoll = setInterval(() => {
        void this._pollLogin();
      }, 2000);
      await this._pollLogin();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private async _pollLogin() {
    if (!this._loginId) return;
    try {
      const result = await this._client.pollLogin(this._loginId);
      if (result.status === 'pending') {
        this._postMessage({ type: 'loginPending' });
        return;
      }
      this._clearLoginPoll();
      this._loginId = undefined;
      this._postMessage({ type: 'loginAuthorized', user: result.user });
      if (this._loginStartedFromCommand) {
        this._postMessage({
          type: 'assistantMessage',
          text: `Signed in as ${result.user?.name || result.user?.username || 'AtomGit user'}.`,
        });
        this._loginStartedFromCommand = false;
      }
      await this._sendSetupState();
    } catch (e) {
      this._clearLoginPoll();
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
      if (this._loginStartedFromCommand) {
        this._postMessage({ type: 'error', message: this._messageFromError(e) });
        this._loginStartedFromCommand = false;
      }
    }
  }

  private async _cancelLogin() {
    this._clearLoginPoll();
    if (this._loginId) {
      const id = this._loginId;
      this._loginId = undefined;
      await this._client.cancelLogin(id).catch(() => undefined);
    }
  }

  private _clearLoginPoll() {
    if (this._loginPoll) {
      clearInterval(this._loginPoll);
      this._loginPoll = undefined;
    }
  }

  private async _ensureLoggedInForCodingPlan(announceInChat = false): Promise<boolean> {
    try {
      const auth = await this._client.authStatus();
      if (auth.logged_in) {
        return true;
      }

      if (announceInChat) {
        this._postMessage({
          type: 'assistantMessage',
          text: 'Opening AtomGit sign-in in your browser. Complete authorization there, then return to VS Code.',
        });
      }
      this._postMessage({ type: 'setupWorking', message: 'Waiting for AtomGit sign-in...' });

      await this._cancelLogin();
      const login = await this._client.startLogin(true);
      this._loginId = login.login_id;
      this._postMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });

      while (this._loginId === login.login_id) {
        const result = await this._client.pollLogin(login.login_id);
        if (result.status === 'pending') {
          this._postMessage({ type: 'loginPending' });
          await delay(2000);
          continue;
        }

        this._loginId = undefined;
        this._postMessage({ type: 'loginAuthorized', user: result.user });
        if (announceInChat) {
          this._postMessage({
            type: 'assistantMessage',
            text: `Signed in as ${result.user?.name || result.user?.username || 'AtomGit user'}.`,
          });
        }
        await this._sendSetupState();
        return true;
      }

      return false;
    } catch (e) {
      this._clearLoginPoll();
      this._loginId = undefined;
      const message = this._messageFromError(e);
      this._postMessage({ type: 'setupError', message });
      if (announceInChat) {
        this._postMessage({ type: 'error', message });
      }
      return false;
    }
  }

  private async _setupCodingPlan(
    options: { loginIfNeeded?: boolean; announceInChat?: boolean } = {},
  ): Promise<CodingPlanSetupResponse | undefined> {
    try {
      if (options.loginIfNeeded) {
        const loggedIn = await this._ensureLoggedInForCodingPlan(options.announceInChat);
        if (!loggedIn) {
          return undefined;
        }
      }

      if (options.announceInChat) {
        this._postMessage({
          type: 'assistantMessage',
          text: 'Syncing CodingPlan models...',
        });
      }
      this._postMessage({ type: 'setupWorking', message: 'Syncing CodingPlan models...' });
      const result: CodingPlanSetupResponse = await this._client.setupCodingPlan(this._loginId);
      this._postMessage({ type: 'codingPlanResult', result });
      await this._sendSetupState();
      return result;
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
      return undefined;
    }
  }

  private async _createProvider(provider: CreateProviderRequest) {
    try {
      await this._client.createProvider(provider);
      await this._sendSetupState();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private async _deleteProvider(name: string) {
    try {
      await this._client.deleteProvider(name);
      await this._sendSetupState();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private async _setDefaultProvider(name: string) {
    if (!name) return;
    try {
      const config = await this._client.setDefaultProvider(name);
      const provider = config.providers.find((p) => p.name === config.default_provider);
      this.onModelSelected?.(provider?.model || config.default_provider);
      await this._sendSetupState();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private async _patchThinking(name: string, thinking: PatchThinkingRequest) {
    try {
      await this._client.patchThinking(name, thinking);
      await this._sendSetupState();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private _sendEditorContext(webview?: vscode.Webview) {
    const editor = vscode.window.activeTextEditor;
    if (editor) {
      const selection = editor.selection;
      this._postMessage({
        type: 'context',
        filePath: editor.document.uri.fsPath,
        fileName: path.basename(editor.document.uri.fsPath),
        selection: !selection.isEmpty ? editor.document.getText(selection) : undefined,
        language: editor.document.languageId,
      }, webview);
    }
  }

  private async _loadSession(sessionId: string, projectHash?: string) {
    // Fix: Skip if already loading this session (prevents duplicate messages
    // when the user double-clicks a session).
    if (this._loadingSessionId === sessionId) return;

    // Don't abort the current session's stream — it keeps running in the
    // background. Its callbacks continue to update its own session runtime
    // via the streamSessionId captured in _handleSend.

    this._loadingSessionId = sessionId;
    try {
      // If projectHash not provided, search sessions to find it
      let hash = projectHash;
      if (!hash) {
        const allSessions = await this._client.listSessions();
        const match = (allSessions as Array<{ project_hash?: string; meta?: { id?: string }; id?: string }>)
          .find(s => (s.meta?.id || s.id) === sessionId);
        hash = match?.project_hash;
        // Fall back to the hash stored in the session runtime — needed for
        // newly created sessions that the daemon filters out (empty messages).
        if (!hash) {
          hash = this._sessionRuntimes.get(sessionId)?.projectHash;
        }
      }
      if (!hash) {
        this._broadcastMessage({ type: 'error', message: 'Unable to load session: missing project hash.' });
        return;
      }

      const detail = await this._client.getSession(hash, sessionId);
      if (detail && detail.messages) {
        this._activeSessionId = sessionId;
        this._loadedMessages = detail.messages;
        this._getRuntime(sessionId).projectHash = hash;
        this.openInTab();
        this._broadcastMessage({ type: 'sessionSelected', sessionId, projectHash: hash });
        // Use _broadcastMessage so both sidebar and tab webviews stay in sync
        this._broadcastMessage({ type: 'clearChat' });
        this._broadcastMessage({ type: 'sessionMessages', messages: detail.messages });

        const newRt = this._sessionRuntimes.get(sessionId);

        // Clear the unread marker now that the user is viewing this session
        if (newRt) {
          newRt.hasUnread = false;
        }

        // If a background stream errored, surface the error now that the
        // user switched back.
        if (newRt?.errorMessage) {
          this._broadcastMessage({ type: 'error', message: newRt.errorMessage });
          newRt.errorMessage = undefined;
        }

        // If this session has an active background stream, replay buffered
        // events so the webview resumes live streaming display.
        if (newRt?.isGenerating && newRt.eventBuffer.length > 0) {
          this._replayStreamBuffer(sessionId, newRt);
        }

        // If the session is idle but has queued messages left over from
        // before the user switched away, drain them now.
        if (newRt && !newRt.isGenerating && newRt.queuedMessages.length > 0) {
          setTimeout(() => void this._sendNextQueuedMessage(), 75);
        }

        // Push updated session list so hasUnread / isGenerating reflect
        // the latest runtime state (e.g. green dots update immediately).
        await this._refreshSessions();

        this.focusInput();
      } else {
        this._broadcastMessage({ type: 'error', message: 'Unable to load session: empty response.' });
      }
    } catch (e) {
      this._broadcastMessage({ type: 'error', message: `Unable to load session: ${this._messageFromError(e)}` });
    } finally {
      if (this._loadingSessionId === sessionId) {
        this._loadingSessionId = undefined;
      }
    }
  }

  // Replay buffered stream events so the webview can resume displaying a
  // background stream. Snapshot the event buffer to avoid conflicts with new
  // events arriving during replay (they will be forwarded by callbacks once
  // _activeSessionId is set).
  private _replayStreamBuffer(_sessionId: string, rt: SessionRuntime, webview?: vscode.Webview) {
    if (!rt.isGenerating || rt.eventBuffer.length === 0) return;

    const post = (msg: unknown) => webview
      ? webview.postMessage(msg)
      : this._broadcastMessage(msg);

    const snapshot = [...rt.eventBuffer];

    for (const evt of snapshot) {
      if (evt.type === 'userMessage') {
        post({ type: 'userMessage', text: evt.data.text });
      }
    }

    post({ type: 'resumeStreaming' });

    for (const evt of snapshot) {
      switch (evt.type) {
        case 'text':
          post({ type: 'text', content: evt.data.content });
          break;
        case 'toolBatchStart':
          post({ type: 'toolBatchStart', calls: evt.data.calls });
          break;
        case 'toolStart':
          post({ type: 'toolStart', id: evt.data.id, name: evt.data.name, args: evt.data.args });
          break;
        case 'toolResult':
          post({ type: 'toolResult', id: evt.data.id, name: evt.data.name, output: evt.data.output, success: evt.data.success, durationMs: evt.data.durationMs });
          break;
        case 'tokens':
          post({ type: 'tokens', prompt: evt.data.prompt, completion: evt.data.completion, total: evt.data.total });
          break;
      }
    }
  }

  private async _renameSession(sessionId: string, projectHash?: string, currentName?: string) {
    const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
    if (!hash) {
      this._postMessage({ type: 'error', message: 'Unable to rename session: missing project hash.' });
      return;
    }

    const nextName = await vscode.window.showInputBox({
      title: 'Rename AtomCode session',
      prompt: 'Enter a new session name',
      value: currentName || '',
      ignoreFocusOut: true,
      validateInput: (value) => value.trim() ? undefined : 'Session name cannot be empty',
    });
    if (nextName === undefined) return;

    try {
      await this._client.renameSession(hash, sessionId, nextName.trim());
      await this._refreshSessions();
    } catch (e) {
      this._postMessage({ type: 'error', message: `Unable to rename session: ${this._messageFromError(e)}` });
    }
  }

  private async _deleteSession(sessionId: string, projectHash?: string, currentName?: string) {
    const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
    if (!hash) {
      this._postMessage({ type: 'error', message: 'Unable to delete session: missing project hash.' });
      return;
    }

    const label = currentName || sessionId;
    const choice = await vscode.window.showWarningMessage(
      `Delete AtomCode session "${label}"?`,
      { modal: true, detail: 'This removes the session from local history.' },
      'Delete',
    );
    if (choice !== 'Delete') return;

    try {
      // Stop any daemon-side stream before deleting
      const rt = this._sessionRuntimes.get(sessionId);
      if (rt?.isGenerating) {
        rt.abortController?.abort();
        void this._client.stopGeneration(sessionId).catch(() => undefined);
      }
      this._sessionRuntimes.delete(sessionId);

      await this._client.deleteSession(hash, sessionId);
      if (this._activeSessionId === sessionId) {
        this._activeSessionId = undefined;
        this._loadedMessages = undefined;
        this._broadcastMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
        this._postMessage({ type: 'clearChat' });
      }
      await this._refreshSessions();
    } catch (e) {
      this._postMessage({ type: 'error', message: `Unable to delete session: ${this._messageFromError(e)}` });
    }
  }

  private async _resolveSessionProjectHash(sessionId: string, projectHash?: string): Promise<string | undefined> {
    if (projectHash) return projectHash;
    try {
      const sessions = await this._client.listSessions();
      const match = (sessions as Array<{ project_hash?: string; meta?: { id?: string }; id?: string }>)
        .find(s => (s.meta?.id || s.id) === sessionId);
      return match?.project_hash;
    } catch {
      return undefined;
    }
  }

  private async _applyCode(code: string, _language: string) {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showInformationMessage('No active editor to apply code to');
      return;
    }
    const selection = editor.selection;
    await editor.edit((editBuilder) => {
      if (selection.isEmpty) {
        editBuilder.insert(selection.active, code);
      } else {
        editBuilder.replace(selection, code);
      }
    });
  }

  private async _handleQuickAction(action: string) {
    const ctx = this._getEditorContext();
    const prompts: Record<string, string> = {
      explain: 'Please explain this code. What does it do and why?',
      fix: 'Please fix any bugs or issues in this code.',
      test: 'Please generate unit tests for this code.',
      refactor: 'Please refactor this code for better readability and maintainability.',
      docs: 'Please add documentation comments to this code.',
      review: 'Please review this code for issues, improvements, and best practices.',
    };
    const prompt = prompts[action] || action;
    const text = ctx.selection
      ? `File: ${ctx.fileName} (${ctx.language})\nSelected code:\n\`\`\`${ctx.language}\n${ctx.selection}\n\`\`\`\n\n${prompt}`
      : prompt;

    this._postMessage({ type: 'userMessage', text: prompt });
    await this._handleSend(text);
  }

  private async _handleSlashCommand(command: string) {
    if (await this._handleLocalCommand(command.trim())) {
      return;
    }

    const mapping: Record<string, string> = {
      '/explain': 'explain',
      '/fix': 'fix',
      '/test': 'test',
      '/refactor': 'refactor',
      '/docs': 'docs',
      '/review': 'review',
    };
    const action = mapping[command];
    if (action) {
      await this._handleQuickAction(action);
    }
  }

  private async _handleLocalCommand(text: string): Promise<boolean> {
    const [command] = text.split(/\s+/, 1);
    switch (command.toLowerCase()) {
      case '/login':
        this._loginStartedFromCommand = true;
        this._postMessage({
          type: 'assistantMessage',
          text: 'Opening AtomGit sign-in in your browser. Complete authorization there, then return to VS Code.',
        });
        await this._startLogin();
        return true;
      case '/codingplan':
        {
          const result = await this._setupCodingPlan({ loginIfNeeded: true, announceInChat: true });
          if (result) {
            this._postMessage({
              type: 'assistantMessage',
              text: '```\n' + result.report_text + '\n```',
            });
          }
        }
        return true;
      default:
        return false;
    }
  }

  private async _searchSessions(query: string) {
    try {
      const sessions = await this._client.searchSessions(query);
      await this._annotateSessionGenerating(sessions as any[]);
      this._broadcastMessage({ type: 'sessions', sessions });
    } catch {}
  }

  private async _annotateSessionGenerating(sessions: Array<{ id?: string; meta?: { id?: string }; isGenerating?: boolean; hasUnread?: boolean }>) {
    // Merge daemon truth (survives extension host reload) with local runtime state
    let activeIds: string[] = [];
    try {
      activeIds = await this._client.activeSessions();
    } catch {}

    for (const s of sessions) {
      const sid = s.meta?.id || s.id;
      if (sid) {
        const rt = this._sessionRuntimes.get(sid);
        s.isGenerating = activeIds.includes(sid) || (rt?.isGenerating ?? false);
        s.hasUnread = rt?.hasUnread ?? false;
      }
    }
  }

  private async _refreshSessions() {
    try {
      const sessions = await this._client.listSessions();
      await this._annotateSessionGenerating(sessions as any[]);
      // If we have an active session that the daemon filtered out (e.g. newly
      // created with no messages yet), prepend a synthetic entry so it appears
      // in the session list immediately.
      if (this._activeSessionId && !sessions.some((s: any) => (s.meta?.id || s.id) === this._activeSessionId)) {
        sessions.unshift({
          id: this._activeSessionId,
          name: 'New session',
          created_at: Date.now(),
          updated_at: Date.now(),
          isGenerating: this._sessionRuntimes.get(this._activeSessionId)?.isGenerating ?? false,
          hasUnread: this._sessionRuntimes.get(this._activeSessionId)?.hasUnread ?? false,
        } as any);
      }
      this._broadcastMessage({ type: 'sessions', sessions });
    } catch {}
  }

  private _getEditorContext() {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return {};
    const selection = editor.selection;
    return {
      filePath: editor.document.uri.fsPath,
      fileName: path.basename(editor.document.uri.fsPath),
      selection: !selection.isEmpty ? editor.document.getText(selection) : undefined,
      language: editor.document.languageId,
    };
  }

  private _postMessage(msg: unknown, webview?: vscode.Webview) {
    if (webview) {
      webview.postMessage(msg);
      return;
    }
    // Chat events should land in one active chat surface to avoid duplicate messages.
    if (this._panel) {
      this._panel.webview.postMessage(msg);
    } else {
      this._view?.webview.postMessage(msg);
    }
  }

  private _broadcastMessage(msg: unknown) {
    this._view?.webview.postMessage(msg);
    this._panel?.webview.postMessage(msg);
  }

  private _markPanelReady(webview: vscode.Webview) {
    if (this._panel?.webview !== webview) return;

    this._panelReady = true;
    this._panelReadyResolver?.();
    this._panelReadyResolver = undefined;
  }

  private async _waitForPanelReady(timeoutMs: number) {
    if (!this._panel) {
      throw new Error('AtomCode panel was not opened.');
    }
    if (this._panelReady) {
      return;
    }

    const readyPromise = this._panelReadyPromise;
    if (!readyPromise) {
      throw new Error('AtomCode panel is not initializing.');
    }

    const ready = await Promise.race([
      readyPromise.then(() => true),
      new Promise<boolean>((resolve) => setTimeout(() => resolve(false), timeoutMs)),
    ]);

    if (!ready) {
      throw new Error('AtomCode panel did not finish initializing.');
    }
  }

  private _messageFromError(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  private _getHtml(webview: vscode.Webview, mode: WebviewMode): string {
    const htmlPath = vscode.Uri.joinPath(this._extensionUri, 'webview', 'index.html');
    const jsPath = vscode.Uri.joinPath(this._extensionUri, 'webview', 'webview.js');
    const cssPath = vscode.Uri.joinPath(this._extensionUri, 'webview', 'webview.css');
    let html = fs.readFileSync(htmlPath.fsPath, 'utf-8');
    const jsVersion = fs.statSync(jsPath.fsPath).mtimeMs.toString(36);
    const cssVersion = fs.statSync(cssPath.fsPath).mtimeMs.toString(36);

    const webviewJsUri = webview.asWebviewUri(jsPath);
    const webviewCssUri = webview.asWebviewUri(cssPath);
    const nonce = getNonce();

    html = html.replace(/\{\{webviewJsUri\}\}/g, `${webviewJsUri.toString()}?v=${jsVersion}`);
    html = html.replace(/\{\{webviewCssUri\}\}/g, `${webviewCssUri.toString()}?v=${cssVersion}`);
    html = html.replace(/\{\{nonce\}\}/g, nonce);
    html = html.replace(/\{\{cspSource\}\}/g, webview.cspSource);
    html = html.replace(/\{\{viewMode\}\}/g, mode);

    return html;
  }
}

function getNonce(): string {
  let text = '';
  const possible = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  for (let i = 0; i < 32; i++) {
    text += possible.charAt(Math.floor(Math.random() * possible.length));
  }
  return text;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
