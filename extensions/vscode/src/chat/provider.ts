import * as vscode from 'vscode';
import { parseOpenFileSelection } from './filePosition';
import * as path from 'path';
import * as fs from 'fs';
import { classifyAuthDisplayState } from '../auth/status';
import { DaemonClient, DaemonHttpError } from '../daemon/client';
import {
  AuthStatusResponse,
  ChatRequest,
  ChatStopReason,
  CodingPlanSetupResponse,
  ConfigResponse,
  CreateProviderRequest,
  ModelInfo,
  MessageInfo,
  PatchThinkingRequest,
  ProvidersResponse,
  ImageInput,
  PermissionDecision,
  SessionMeta,
} from '../daemon/types';
import { getQuickActionPrompt } from './quickActions';
import {
  ApprovalMode,
  ApprovalModeState,
  beginApprovalModeSwitch,
  completeApprovalModeSwitch,
  failApprovalModeSwitch,
  initApprovalModeState,
} from './modeState';

type WebviewMode = 'sidebar' | 'tab';
type ContextItem = { path: string; type: string; fileName?: string; language?: string; selection?: string; startLine?: number; endLine?: number };
type QueuedChatMessage = { text: string; context?: ContextItem[]; images?: ImageInput[]; clientMessageId?: string; approvalMode?: ApprovalMode };
type WorkspacePathItem = { path: string; fileName: string; relativePath: string; isDir: boolean; depth: number };
type PanelSessionInfo = {
  sessionId: string;
  projectHash?: string;
  workingDir?: string;
  messages?: MessageInfo[];
  messagesPromise?: Promise<MessageInfo[] | undefined>;
};

type SessionTerminal = (
  | { type: 'done'; tokens: number; toolCalls: number; sessionId?: string; stopReason?: ChatStopReason; message?: string }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
) & { generation: number };

type PendingPanelMessage = {
  message: any;
  generation?: number;
};

type PanelHistoryLoad = {
  sessionId: string;
  projectHash: string;
  promise: Promise<MessageInfo[] | undefined>;
};

type SessionMetaLike = SessionMeta & {
  isGenerating?: boolean;
  hasUnread?: boolean;
};

type LoadedSessionsForDisplay = {
  sessions: SessionMetaLike[];
  currentProjectHash?: string;
  workspaceFolder?: string;
};

function sessionUpdatedAt(session: SessionMetaLike): number {
  return typeof session.updated_at === 'number' ? session.updated_at : 0;
}

function sessionKey(session: SessionMetaLike, fallbackProjectHash?: string): string {
  return `${session.project_hash || fallbackProjectHash || session.working_dir || 'unknown'}:${session.id}`;
}

export function mergeSessionsForDisplay(
  globalSessions: SessionMetaLike[],
  currentProjectSessions: SessionMetaLike[],
  currentProjectHash?: string,
): SessionMetaLike[] {
  if (currentProjectHash) {
    const current = new Map<string, SessionMetaLike>();
    for (const session of currentProjectSessions) {
      const withProjectHash = {
        ...session,
        project_hash: session.project_hash || currentProjectHash,
      };
      current.set(sessionKey(withProjectHash, currentProjectHash), withProjectHash);
    }
    return Array.from(current.values()).sort((a, b) => sessionUpdatedAt(b) - sessionUpdatedAt(a));
  }

  const merged = new Map<string, SessionMetaLike>();
  for (const session of globalSessions) {
    merged.set(sessionKey(session), session);
  }
  for (const session of currentProjectSessions) {
    const withProjectHash = {
      ...session,
      project_hash: session.project_hash || currentProjectHash,
    };
    merged.set(sessionKey(withProjectHash, currentProjectHash), withProjectHash);
  }
  return Array.from(merged.values()).sort((a, b) => sessionUpdatedAt(b) - sessionUpdatedAt(a));
}

interface SessionRuntime {
  abortController?: AbortController;
  isGenerating: boolean;
  streamGeneration?: number;
  queuedMessages: QueuedChatMessage[];
  projectHash?: string;
  terminalSeen?: boolean;
  terminal?: SessionTerminal;
  recoveryLocked?: boolean;
  messages?: MessageInfo[];
  // 进行中的活跃会话轮询句柄，用于去重避免双重轮询
  pollHandle?: { cancelled: boolean };
  eventBuffer: Array<{
    type: 'userMessage' | 'text' | 'toolBatchStart' | 'toolStart' | 'toolProgress' | 'toolResult' | 'permissionRequest' | 'artifactStart' | 'artifactContent' | 'artifactEnd' | 'warning' | 'persistenceWarning' | 'rateLimited' | 'tokens';
    data: any;
  }>;
}

interface StreamReplayCursor {
  sessionId?: string;
  streamGeneration: number;
  replayedEvents: number;
  historyGeneration?: number;
  terminalGeneration?: number;
}

function isDestructivePermissionTool(toolName: string): boolean {
  const normalized = toolName.toLowerCase();
  return normalized.includes('bash')
    || normalized.includes('execute')
    || normalized.includes('write')
    || normalized.includes('edit')
    || normalized.includes('replace')
    || normalized.includes('delete')
    || normalized.includes('parallel_edit');
}

function isPermissionDecision(value: unknown): value is PermissionDecision {
  return value === 'allow'
    || value === 'deny'
    || value === 'always_allow'
    || value === 'allow_persist';
}

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _panels = new Map<string, vscode.WebviewPanel>();
  private _webviewPanels = new Map<vscode.Webview, vscode.WebviewPanel>();
  private _sessionBindingPromises = new Map<vscode.Webview, Promise<string | undefined>>();
  private _panelHistoryLoads = new WeakMap<vscode.WebviewPanel, PanelHistoryLoad>();
  private _panelSessions = new Map<string, PanelSessionInfo>();
  private _panelReady = new Map<string, boolean>();
  private _activeSessionId?: string;
  private _focusedPanelId?: string;
  private _sessionRuntimes = new Map<string, SessionRuntime>();
  private _pendingMessages = new Map<string, PendingPanelMessage[]>();
  private _loginId?: string;
  private _loginGeneration = 0;
  private _loginInFlight = false;
  private _loginStartedFromCommand = false;
  private _workspacePathCache?: { root: string; builtAt: number; items: WorkspacePathItem[] };
  private _approvalModeState: ApprovalModeState = initApprovalModeState('build');
  public onModelSelected?: (model: string) => void;

  private _settingsWatcher?: vscode.Disposable;
  private _atomCodeConfigWatcher?: vscode.FileSystemWatcher;
  private _atomCodeAuthWatcher?: vscode.FileSystemWatcher;
  private _watchedConfigPath?: string;
  private _watchedAuthPath?: string;
  private _setupRefreshTimer?: NodeJS.Timeout;
  private _setupStateGeneration = 0;

  constructor(
    private readonly _extensionUri: vscode.Uri,
    private readonly _client: DaemonClient,
  ) {
    // Apply a chat-font config change LIVE (set the CSS var in the DOM) instead of rebuilding
    // the webview HTML — a rebuild would reset the active chat. Initial load is handled by the
    // `{{fontStyle}}` injection in `_getHtml`.
    this._settingsWatcher = vscode.workspace.onDidChangeConfiguration((e) => {
      if (
        e.affectsConfiguration('atomcode.chat.fontFamily') ||
        e.affectsConfiguration('chatEditor.fontFamily')
      ) {
        this._broadcastChromeFont();
      }
    });
  }

  private _broadcastChromeFont() {
    const msg = { type: 'chromeFont', value: resolveChatFontFamily() ?? null };
    this._view?.webview.postMessage(msg);
    for (const webview of this._webviewPanels.keys()) {
      webview.postMessage(msg);
    }
  }

  private async _loadSessionsForDisplay(): Promise<LoadedSessionsForDisplay> {
    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (workspaceFolder) {
      try {
        const currentProjectSessions = await this._client.listSessionsForWorkingDir(workspaceFolder) as SessionMetaLike[];
        const currentProjectHash = currentProjectSessions.find((session) => session.project_hash)?.project_hash;
        return {
          sessions: currentProjectHash
            ? mergeSessionsForDisplay([], currentProjectSessions, currentProjectHash)
            : currentProjectSessions,
          currentProjectHash,
          workspaceFolder,
        };
      } catch {
        // Fall back to the global list only if the scoped endpoint is unavailable.
      }
    }

    const globalSessions = await this._client.listSessions() as SessionMetaLike[];
    if (!workspaceFolder) return { sessions: globalSessions };

    // Older daemons may not expose the scoped endpoint. Their global fallback
    // is still usable, but must not leak sessions from unrelated workspaces
    // into the current sidebar.
    const resolvedWorkspace = path.resolve(workspaceFolder);
    const currentProjectSessions = globalSessions.filter((session) =>
      session.working_dir
        ? path.resolve(session.working_dir) === resolvedWorkspace
        : false
    );
    return {
      sessions: currentProjectSessions,
      currentProjectHash: currentProjectSessions.find((session) => session.project_hash)?.project_hash,
      workspaceFolder,
    };
  }

  public dispose() {
    this._setupStateGeneration += 1;
    this._loginGeneration += 1;
    const loginId = this._loginId;
    this._loginId = undefined;
    if (loginId) void this._client.cancelLogin(loginId).catch(() => undefined);
    this._settingsWatcher?.dispose();
    this._atomCodeConfigWatcher?.dispose();
    this._atomCodeAuthWatcher?.dispose();
    if (this._setupRefreshTimer) clearTimeout(this._setupRefreshTimer);
  }

  private _findAtomCodeTabGroup(): vscode.ViewColumn | undefined {
    for (const group of vscode.window.tabGroups.all) {
      if (group.tabs.some(t => t.input instanceof vscode.TabInputWebview
            && (t.input as vscode.TabInputWebview).viewType.includes('atomcode.chatTab'))) {
        return group.viewColumn;
      }
    }
    return undefined;
  }

  public openInTab(sessionId?: string) {
    // If session is already open in a panel, reveal it
    if (sessionId) {
      const existing = this._panels.get(sessionId);
      if (existing) {
        existing.reveal();
        this._focusedPanelId = sessionId;
        this._activeSessionId = sessionId;
        return;
      }
    }

    const column = this._findAtomCodeTabGroup() ?? vscode.ViewColumn.Beside;

    const panel = vscode.window.createWebviewPanel(
      'atomcode.chatTab',
      'AtomCode',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [
          vscode.Uri.joinPath(this._extensionUri, 'webview'),
          vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
        ],
      },
    );

    panel.iconPath = {
      light: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
      dark: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
    };
    const webview = panel.webview;
    this._webviewPanels.set(webview, panel);

    // Track the panel — required for message routing, size check, and lookup
    if (sessionId) {
      this._panels.set(sessionId, panel);
      this._focusedPanelId = sessionId;
      this._activeSessionId = sessionId;
    }

    // Bind the ready listener only after the panel/session indexes exist, but
    // before assigning HTML. The webview may boot quickly enough to emit its
    // one-shot ready event as soon as the document is installed.
    this._setupWebviewMessageHandler(webview, 'tab');
    webview.html = this._getHtml(webview, 'tab');

    // When user switches to this tab in VS Code, sync sidebar selection
    panel.onDidChangeViewState((e) => {
      if (e.webviewPanel.active) {
        void this._sendSetupState(webview);
        const activeSid = this._findSessionIdByPanel(panel);
        if (activeSid) {
          this._focusedPanelId = activeSid;
          const info = this._panelSessions.get(activeSid);
          this._selectSession(activeSid, info?.projectHash);
        }
      }
    });

    panel.onDidDispose(() => {
      this._handlePanelDisposed(panel, webview);
    });
  }

  private _findSessionIdByPanel(panel: vscode.WebviewPanel): string | undefined {
    for (const [sid, p] of this._panels) {
      if (p === panel) return sid;
    }
    return undefined;
  }

  private _sessionIdForWebview(webview: vscode.Webview): string | undefined {
    const panel = this._webviewPanels.get(webview);
    return panel ? this._findSessionIdByPanel(panel) : undefined;
  }

  private _handlePanelDisposed(panel: vscode.WebviewPanel, webview: vscode.Webview) {
    const disposedSid = this._findSessionIdByPanel(panel);
    this._webviewPanels.delete(webview);
    this._sessionBindingPromises.delete(webview);
    this._panelHistoryLoads.delete(panel);
    if (!disposedSid) return;

    const rt = this._sessionRuntimes.get(disposedSid);
    this._panels.delete(disposedSid);
    this._panelReady.delete(disposedSid);
    this._pendingMessages.delete(disposedSid);
    this._panelSessions.delete(disposedSid);
    if (this._focusedPanelId === disposedSid) {
      this._focusedPanelId = undefined;
    }

    // 保留 queuedMessages：窗口关闭不应丢弃排队中的请求。
    // 队列在以下场景被显式清空：
    //   - stopGeneration（用户主动停止）
    //   - _deleteSessionInternal（会话被删除）
    //   - onDone with !completedNormally（异常终止）
    //   - onStopped / onError（流终止/出错）
    if (rt?.isGenerating || rt?.recoveryLocked) {
      rt.terminalSeen = true;
      rt.isGenerating = false;
      rt.recoveryLocked = true;
      rt.terminal = { type: 'stopped', generation: rt.streamGeneration ?? 0 };
      rt.abortController?.abort();
      rt.abortController = undefined;
      const generation = rt.streamGeneration ?? 0;
      void this._client.stopGeneration(disposedSid).then((result) => {
        if (!result.success) throw new Error(result.message || 'stop request failed');
        const current = this._sessionRuntimes.get(disposedSid);
        if (current?.streamGeneration === generation) current.recoveryLocked = false;
      }).catch(() => {
        const current = this._sessionRuntimes.get(disposedSid);
        if (current?.streamGeneration === generation) current.recoveryLocked = true;
      });
    }
  }

  private _selectSession(sessionId?: string, projectHash?: string) {
    this._activeSessionId = sessionId;
    const message = { type: 'sessionSelected', sessionId, projectHash };
    if (sessionId) {
      this._postOrQueueToPanel(sessionId, message);
    } else {
      this._view?.webview.postMessage(message);
    }
  }

  private async _ensureSessionForWebview(webview: vscode.Webview): Promise<string | undefined> {
    const bound = this._sessionIdForWebview(webview);
    if (bound) return bound;

    const inFlight = this._sessionBindingPromises.get(webview);
    if (inFlight) return inFlight;

    const binding = (async () => {
      const panel = this._webviewPanels.get(webview);
      if (!panel) return undefined;
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const session = await this._client.createSession(undefined, workspaceFolder);
      if (this._webviewPanels.get(webview) !== panel) return undefined;

      const rt = this._getRuntime(session.id);
      rt.projectHash = session.project_hash;
      this._panels.set(session.id, panel);
      this._panelReady.set(session.id, true);
      this._panelSessions.set(session.id, {
        sessionId: session.id,
        projectHash: session.project_hash,
        workingDir: session.working_dir,
      });
      this._focusedPanelId = session.id;
      this._selectSession(session.id, session.project_hash);
      await this._refreshSessions();
      if (
        this._webviewPanels.get(webview) !== panel
        || this._sessionIdForWebview(webview) !== session.id
      ) {
        return undefined;
      }
      return session.id;
    })();
    this._sessionBindingPromises.set(webview, binding);
    try {
      return await binding;
    } finally {
      if (this._sessionBindingPromises.get(webview) === binding) {
        this._sessionBindingPromises.delete(webview);
      }
    }
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

  public async openForEditorCommand(sessionId?: string) {
    this.openInTab(sessionId);
    // Wait briefly for the panel to be ready
    await new Promise(resolve => setTimeout(resolve, 500));
  }

  private _loadPanelHistory(
    panel: vscode.WebviewPanel,
    sessionId: string,
    projectHash: string,
  ): Promise<MessageInfo[] | undefined> {
    const currentLoad = this._panelHistoryLoads.get(panel);
    if (
      currentLoad?.sessionId === sessionId
      && currentLoad.projectHash === projectHash
      && this._panels.get(sessionId) === panel
    ) {
      return currentLoad.promise;
    }

    const startGeneration = this._sessionRuntimes.get(sessionId)?.streamGeneration ?? 0;
    let promise!: Promise<MessageInfo[] | undefined>;
    promise = this._client.getSession(projectHash, sessionId)
      .then((detail) => {
        const activeLoad = this._panelHistoryLoads.get(panel);
        const runtime = this._sessionRuntimes.get(sessionId);
        if (
          activeLoad?.promise !== promise
          || this._panels.get(sessionId) !== panel
          || (runtime?.streamGeneration ?? 0) !== startGeneration
          || runtime?.isGenerating
        ) {
          return undefined;
        }

        const info = this._panelSessions.get(sessionId);
        if (
          info?.messagesPromise !== promise
          || (info.projectHash && info.projectHash !== projectHash)
        ) {
          return undefined;
        }

        const messages = detail.messages;
        this._panelSessions.set(sessionId, {
          ...info,
          sessionId,
          projectHash,
          messages,
          messagesPromise: undefined,
        });
        const targetRuntime = this._getRuntime(sessionId);
        targetRuntime.projectHash = projectHash;
        targetRuntime.messages = messages;
        this._postOrQueueToPanel(sessionId, {
          type: 'sessionMessages',
          messages,
          terminal: this._terminalForWebview(targetRuntime.terminal),
        }, targetRuntime.streamGeneration ?? 0);
        return messages;
      })
      .finally(() => {
        if (this._panelHistoryLoads.get(panel)?.promise === promise) {
          this._panelHistoryLoads.delete(panel);
        }
        const info = this._panelSessions.get(sessionId);
        if (info?.messagesPromise === promise) {
          this._panelSessions.set(sessionId, {
            ...info,
            messagesPromise: undefined,
          });
        }
      });

    this._panelHistoryLoads.set(panel, { sessionId, projectHash, promise });
    const info = this._panelSessions.get(sessionId);
    this._panelSessions.set(sessionId, {
      ...info,
      sessionId,
      projectHash,
      messages: info?.projectHash === projectHash ? info.messages : undefined,
      messagesPromise: promise,
    });
    return promise;
  }

  private async _restorePanelHistory(
    panel: vscode.WebviewPanel,
    sessionId: string,
    projectHash?: string,
  ): Promise<void> {
    try {
      const resolvedProjectHash = await this._resolveSessionProjectHash(sessionId, projectHash);
      if (!resolvedProjectHash) return;
      if (this._panels.get(sessionId) !== panel) return;
      await this._loadPanelHistory(panel, sessionId, resolvedProjectHash);
    } catch {
      // Keep the restored tab retryable. A later session-list click carries an
      // explicit project hash and re-enters the same guarded loader.
    }
  }

  public setupPanelForRestore(panel: vscode.WebviewPanel, sessionId?: string, projectHash?: string) {
    const webview = panel.webview;
    webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this._extensionUri, 'webview'),
        vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
      ],
    };
    panel.iconPath = {
      light: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
      dark: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
    };
    this._webviewPanels.set(webview, panel);

    // Track the panel
    if (sessionId) {
      this._panels.set(sessionId, panel);
    }

    if (sessionId) {
      void this._restorePanelHistory(panel, sessionId, projectHash);
    }

    // Complete the restored session binding before the document can emit
    // `ready`, otherwise initial history can be requested without an owner.
    this._setupWebviewMessageHandler(webview, 'tab');
    webview.html = this._getHtml(webview, 'tab');

    // When user switches to this tab in VS Code, sync sidebar selection
    panel.onDidChangeViewState((e) => {
      if (e.webviewPanel.active) {
        void this._sendSetupState(webview);
        const activeSid = this._findSessionIdByPanel(panel);
        if (activeSid) {
          this._focusedPanelId = activeSid;
          const info = this._panelSessions.get(activeSid);
          this._selectSession(activeSid, info?.projectHash);
        }
      }
    });

    panel.onDidDispose(() => {
      this._handlePanelDisposed(panel, webview);
    });
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
      if (webviewView.visible) void this._sendSetupState(webviewView.webview);
    });
  }

  private _setupWebviewMessageHandler(webview: vscode.Webview, mode: WebviewMode) {
    webview.onDidReceiveMessage(async (msg) => {
      switch (msg.type) {
        case 'send': {
          const targetSessionId = mode === 'tab'
            ? (this._sessionIdForWebview(webview) ?? await this._ensureSessionForWebview(webview))
            : msg.sessionId;
          if (mode === 'tab' && !targetSessionId) break;
          await this._handleSend(
            msg.text,
            msg.context,
            msg.images,
            msg.clientMessageId,
            targetSessionId,
            msg.approvalMode,
            mode === 'tab' ? webview : undefined,
          );
          break;
        }
        case 'stop':
          this.stopGeneration(
            mode === 'tab' ? this._sessionIdForWebview(webview) : msg.sessionId,
          );
          break;
        case 'newConversation':
          await this.newConversation();
          break;
        case 'ready':
          this._markPanelNotReady(webview);
          {
            const replayCursor = await this._sendInitialState(webview, mode);
            this._finishPanelReadyReplay(webview, replayCursor);
          }
          break;
        case 'selectModel':
          await this._setDefaultProvider(msg.provider || msg.model);
          break;
        case 'selectReasoningEffort':
          await this._setReasoningEffort(msg.provider, msg.effort);
          break;
        case 'selectApprovalMode':
          await this._setApprovalMode(msg.mode);
          break;
        case 'permissionResponse':
          await this._handlePermissionResponse(msg);
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
        case 'openSessionInTab':
          await this.openSessionInTab(msg.sessionId, msg.projectHash);
          break;
        case 'loadSession':
          await this.openSessionInTab(msg.sessionId, msg.projectHash);
          break;
        case 'renameSession':
          await this._renameSession(msg.sessionId, msg.projectHash, msg.name);
          break;
        case 'deleteSession':
          await this._deleteSession(msg.sessionId, msg.projectHash, msg.name);
          break;
        case 'deleteSessions':
          await this._deleteSessions(msg.sessions, webview);
          break;
        case 'openSidebar':
          await this.openInSidebar();
          break;
        case 'openSettings':
          vscode.commands.executeCommand('workbench.action.openSettings', 'atomcode');
          break;
        case 'openFile':
          if (msg.path) {
            const uri = vscode.Uri.file(msg.path);
            const opts: vscode.TextDocumentShowOptions = {
              viewColumn: vscode.ViewColumn.Active,
              preserveFocus: false,
            };
            const selection = parseOpenFileSelection(msg);
            if (selection) {
              opts.selection = new vscode.Range(
                selection.startLine - 1,
                selection.startColumn - 1,
                selection.endLine - 1,
                selection.endColumn - 1,
              );
            }
            // Try to reveal in existing editor if already open
            const existingEditor = vscode.window.visibleTextEditors.find(
              (e) => e.document.uri.fsPath === msg.path
            );
            if (existingEditor) {
              if (opts.selection) {
                existingEditor.selection = new vscode.Selection(opts.selection.start, opts.selection.end);
              }
              vscode.window.showTextDocument(existingEditor.document, {
                viewColumn: existingEditor.viewColumn,
                selection: opts.selection,
              });
            } else {
              vscode.window.showTextDocument(uri, opts);
            }
          }
          break;
        case 'applyCode':
          await this._applyCode(msg.code, msg.language);
          break;
        case 'copyCode':
          vscode.env.clipboard.writeText(msg.code);
          break;
        case 'quickAction': {
          const targetSessionId = mode === 'tab'
            ? (this._sessionIdForWebview(webview) ?? await this._ensureSessionForWebview(webview))
            : msg.sessionId;
          if (mode === 'tab' && !targetSessionId) break;
          await this._handleQuickAction(
            msg.action,
            targetSessionId,
            mode === 'tab' ? webview : undefined,
          );
          break;
        }
        case 'slashCommand': {
          const targetSessionId = mode === 'tab'
            ? (this._sessionIdForWebview(webview) ?? await this._ensureSessionForWebview(webview))
            : msg.sessionId;
          if (mode === 'tab' && !targetSessionId) break;
          await this._handleSlashCommand(
            msg.command,
            targetSessionId,
            mode === 'tab' ? webview : undefined,
          );
          break;
        }
        case 'getSkills':
          try {
            const skills = await this._client.listSkills();
            this._postMessage({ type: 'skills', skills }, webview);
          } catch {
            this._postMessage({ type: 'skills', skills: [] }, webview);
          }
          break;
        case 'searchSessions':
          await this._searchSessions(msg.query);
          break;
        case 'popout':
          this.openInTab();
          break;
        case 'attachFile': {
          if (msg.path) {
            // File already selected from the webview file picker — just attach it
            const filePath = msg.path;
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
        case 'pickPathForInsert': {
          const uris = await vscode.window.showOpenDialog({
            canSelectFiles: true,
            canSelectFolders: true,
            canSelectMany: false,
            openLabel: vscode.l10n.t('Insert Path'),
          });
          const picked = uris?.[0]?.fsPath;
          if (picked) {
            this._postMessage({ type: 'insertText', text: `${picked} ` }, webview);
          }
          break;
        }
        case 'pickContextFile': {
          const uris = await vscode.window.showOpenDialog({
            canSelectFiles: true,
            canSelectFolders: false,
            canSelectMany: true,
            openLabel: vscode.l10n.t('Attach File'),
          });
          for (const uri of uris ?? []) {
            this._postMessage({
              type: 'context',
              filePath: uri.fsPath,
              fileName: path.basename(uri.fsPath),
              language: '',
            }, webview);
          }
          break;
        }
        case 'searchWorkspaceFiles': {
          const query = String(msg.query || '').trim();
          const workspaceFolder = vscode.workspace.workspaceFolders?.[0];
          if (!workspaceFolder) {
            this._postMessage({ type: 'workspaceFiles', files: [], query });
            break;
          }
          // Build glob: if user typed "foo", match "**/*foo*" across the workspace,
          // excluding common noise directories.
          const pattern = query ? `**/*${query}*` : '**/*';
          const excludePattern = '{**/node_modules/**,**/.git/**,**/target/**,**/dist/**,**/build/**,**/__pycache__/**,**/*.d.ts,**/*.map}';
          const uris = await vscode.workspace.findFiles(pattern, excludePattern, 30);
          const files = uris.map((uri) => {
            const relativePath = path.relative(workspaceFolder.uri.fsPath, uri.fsPath);
            return {
              path: uri.fsPath,
              fileName: path.basename(uri.fsPath),
              relativePath,
            };
          });
          this._postMessage({ type: 'workspaceFiles', files, query });
          break;
        }
        case 'searchWorkspacePaths': {
          const query = String(msg.query || '');
          const workspaceFolder = vscode.workspace.workspaceFolders?.[0];
          if (!workspaceFolder) {
            this._postMessage({ type: 'workspacePaths', paths: [], query });
            break;
          }
          try {
            const paths = await this._searchWorkspacePaths(workspaceFolder, query);
            this._postMessage({ type: 'workspacePaths', paths, query });
          } catch {
            this._postMessage({ type: 'workspacePaths', paths: [], query });
          }
          break;
        }
      }
    });
  }

  private async _workspacePathItems(workspaceFolder: vscode.WorkspaceFolder): Promise<WorkspacePathItem[]> {
    const root = workspaceFolder.uri.fsPath;
    const now = Date.now();
    if (this._workspacePathCache
      && this._workspacePathCache.root === root
      && now - this._workspacePathCache.builtAt < 3000) {
      return this._workspacePathCache.items;
    }

    const excludePattern = '{**/node_modules/**,**/.git/**,**/target/**,**/dist/**,**/build/**,**/__pycache__/**,**/*.d.ts,**/*.map}';
    const uris = await vscode.workspace.findFiles('**/*', excludePattern, 50000);
    const byRelativePath = new Map<string, WorkspacePathItem>();
    const toRelative = (fsPath: string) => path.relative(root, fsPath).split(path.sep).join('/');
    const addDir = (relativeDir: string) => {
      const clean = relativeDir.replace(/\/+$/, '');
      if (!clean || clean.includes(' ') || clean === '.git' || clean.startsWith('.git/')) return;
      const relativePath = `${clean}/`;
      if (byRelativePath.has(relativePath)) return;
      byRelativePath.set(relativePath, {
        path: path.join(root, clean),
        fileName: path.basename(clean),
        relativePath,
        isDir: true,
        depth: clean.split('/').filter(Boolean).length,
      });
    };

    for (const uri of uris) {
      const relative = toRelative(uri.fsPath);
      if (!relative || relative.includes(' ') || relative === '.git' || relative.startsWith('.git/')) {
        continue;
      }
      const parts = relative.split('/');
      for (let i = 1; i < parts.length; i += 1) {
        addDir(parts.slice(0, i).join('/'));
      }
      byRelativePath.set(relative, {
        path: uri.fsPath,
        fileName: path.basename(uri.fsPath),
        relativePath: relative,
        isDir: false,
        depth: parts.length,
      });
    }

    const items = Array.from(byRelativePath.values());
    this._workspacePathCache = { root, builtAt: now, items };
    return items;
  }

  private async _searchWorkspacePaths(
    workspaceFolder: vscode.WorkspaceFolder,
    token: string,
  ): Promise<Array<Omit<WorkspacePathItem, 'depth'>>> {
    const slash = token.lastIndexOf('/');
    const scopeDir = slash >= 0 ? token.slice(0, slash + 1) : '';
    const filter = slash >= 0 ? token.slice(slash + 1) : token;
    const filterLower = filter.toLowerCase();
    const scopeDepth = scopeDir ? scopeDir.split('/').filter(Boolean).length : 0;
    const items = await this._workspacePathItems(workspaceFolder);

    return items
      .filter((item) => item.relativePath.startsWith(scopeDir))
      .filter((item) => item.relativePath !== scopeDir)
      .filter((item) => {
        if (!filterLower) return item.depth === scopeDepth + 1;
        return item.relativePath.slice(scopeDir.length).toLowerCase().includes(filterLower);
      })
      .sort((a, b) => {
        const aDirect = a.depth === scopeDepth + 1;
        const bDirect = b.depth === scopeDepth + 1;
        if (aDirect !== bDirect) return aDirect ? -1 : 1;
        if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
        return a.relativePath.localeCompare(b.relativePath);
      })
      .slice(0, 30)
      .map(({ depth, ...item }) => item);
  }

  // Public API for commands
  public async sendMessage(text: string) {
    let sid = this._focusedPanelId;
    if (!sid) {
      // No open panel — start a fresh conversation/tab first. newConversation
      // sets _focusedPanelId to the new session.
      await this.newConversation();
      sid = this._focusedPanelId;
      if (!sid) return;
    }
    // Echo the message into the (possibly freshly-opened) panel via the
    // queueing path — a brand-new tab's webview hasn't sent `ready` yet, so a
    // direct post would be dropped — then actually run the turn. Without
    // _handleSend the text would only render as a bubble and never reach the
    // backend. Mirrors sendEditorCommandMessage.
    this._postOrQueueToPanel(sid, { type: 'userMessage', text });
    await this._handleSend(text);
  }

  public async sendEditorCommandMessage(text: string) {
    let sid = this._focusedPanelId;

    if (!sid) {
      // Create daemon session first, then open tab with sessionId
      // so the panel is properly tracked for message routing.
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const session = await this._client.createSession(undefined, workspaceFolder);
      sid = session.id;
      this._getRuntime(sid).projectHash = session.project_hash;
      this._panelSessions.set(sid, { sessionId: sid, projectHash: session.project_hash, workingDir: session.working_dir });
      this.openInTab(sid);
      await this._refreshSessions();
    } else {
      const rt = this._sessionRuntimes.get(sid);
      if (rt?.isGenerating) {
        this.stopGeneration();
      }
      await this._ensureSession(sid);
      this._postMessageForSession(sid, { type: 'clearChat' });
    }

    this._postOrQueueToPanel(sid!, { type: 'userMessage', text });
    await this._handleSend(text);
  }

  /**
   * Add selected code as a context reference in the chat input.
   * Shows as a clickable file:line-range pill.
   */
  public async addToChat(file: { path: string; fileName: string; language?: string; selection?: string; startLine?: number; endLine?: number }) {
    if (!file.selection) return;
    let sid = this._focusedPanelId;

    if (!sid) {
      // Create daemon session first, then open tab with sessionId
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const session = await this._client.createSession(undefined, workspaceFolder);
      sid = session.id;
      this._getRuntime(sid).projectHash = session.project_hash;
      this._panelSessions.set(sid, { sessionId: sid, projectHash: session.project_hash, workingDir: session.working_dir });
      this.openInTab(sid);
      await this._refreshSessions();
    }

    this._postOrQueueToPanel(sid, {
      type: 'context',
      filePath: file.path,
      fileName: file.fileName,
      language: file.language,
      selection: file.selection,
      startLine: file.startLine,
      endLine: file.endLine,
    });
    this.focusInput();
  }

  public async newConversation() {
    let sessionId: string | undefined;
    let projectHash: string | undefined;

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    let session;
    try {
      session = await this._client.createSession(undefined, workspaceFolder);
    } catch {
      this._selectSession(undefined, undefined);
      return;
    }
    sessionId = session.id;
    projectHash = session.project_hash;
    this._getRuntime(sessionId).projectHash = session.project_hash;
    this._activeSessionId = sessionId;

    // Open new tab for the new session
    this._panelSessions.set(sessionId, { sessionId, projectHash, workingDir: session.working_dir });
    this.openInTab(sessionId);

    this._selectSession(sessionId, projectHash);
    await this._refreshSessions();
  }

  public stopGeneration(sessionId?: string) {
    const sid = sessionId ?? this._focusedPanelId;
    if (!sid) return;
    const rt = this._sessionRuntimes.get(sid);
    if (!rt || (!rt.isGenerating && !rt.recoveryLocked)) return;

    rt.terminalSeen = true;
    rt.abortController?.abort();
    rt.abortController = undefined;
    rt.queuedMessages = [];
    rt.isGenerating = false;
    rt.recoveryLocked = true;
    const stopGeneration = rt.streamGeneration ?? 0;
    rt.terminal = { type: 'stopped', generation: stopGeneration };
    this._postTerminalForSession(sid, { type: 'recoveryRequired' }, stopGeneration);
    void this._client.stopGeneration(sid).then((result) => {
      if (!result.success) throw new Error(result.message || 'stop request failed');
      const current = this._sessionRuntimes.get(sid);
      if (!current || current.streamGeneration !== stopGeneration) return;
      current.recoveryLocked = false;
      this._postTerminalForSession(sid, { type: 'generationStopped' }, stopGeneration);
      this._postTerminalForSession(sid, { type: 'recoveryCleared' }, stopGeneration);
    }).catch((error) => {
      const current = this._sessionRuntimes.get(sid);
      if (!current || current.streamGeneration !== stopGeneration) return;
      current.recoveryLocked = true;
      current.terminal = {
        type: 'error',
        generation: stopGeneration,
        message: `Unable to confirm the turn stopped: ${this._messageFromError(error)}`,
      };
      this._postTerminalForSession(sid, this._terminalForWebview(current.terminal), stopGeneration);
      this._postTerminalForSession(sid, { type: 'recoveryRequired' }, stopGeneration);
    });
  }

  public focusInput() {
    const sid = this._focusedPanelId;
    if (sid) {
      this._postMessage({ type: 'focusInput' });
    }
  }

  // Private
  private _getRuntime(sessionId: string): SessionRuntime {
    let rt = this._sessionRuntimes.get(sessionId);
    if (!rt) {
      rt = { isGenerating: false, streamGeneration: 0, queuedMessages: [], eventBuffer: [] };
      this._sessionRuntimes.set(sessionId, rt);
    }
    return rt;
  }

  private async _handleSend(
    text: string,
    context?: Array<{ path: string; type: string; fileName?: string; language?: string; selection?: string; startLine?: number; endLine?: number }>,
    images?: ImageInput[],
    clientMessageId?: string,
    msgSessionId?: string,
    approvalMode?: ApprovalMode,
    ownerWebview?: vscode.Webview,
  ) {
    const trimmed = text.trim();
    const attachedImages = images?.length ? images : undefined;
    if (!trimmed && !attachedImages) return;

    let sid = msgSessionId ?? this._focusedPanelId;
    if (!sid) {
      sid = await this._ensureSession();
    }
    if (!sid) return;
    const ownerStillBound = () => !ownerWebview || (
      this._webviewPanels.has(ownerWebview)
      && this._sessionIdForWebview(ownerWebview) === sid
    );
    if (!ownerStillBound()) return;
    const rt = this._getRuntime(sid);
    this._activeSessionId = sid;

    if (rt.recoveryLocked) {
      try {
        const activeSessions = await this._client.activeSessions();
        if (!ownerStillBound()) return;
        if (activeSessions.includes(sid)) {
          // 旧回合仍在 daemon 运行 → 入队等待，而非报错丢弃。
          rt.queuedMessages.push({
            text: trimmed,
            context,
            images: attachedImages,
            clientMessageId,
            approvalMode,
          });
          if (clientMessageId) {
            this._postMessageForSession(sid, { type: 'queuedMessageSent', id: clientMessageId });
          }
          return;
        }
        rt.recoveryLocked = false;
      } catch {
        // daemon 不可达 → 入队等待重试，而非报错丢弃。
        rt.queuedMessages.push({
          text: trimmed,
          context,
          images: attachedImages,
          clientMessageId,
          approvalMode,
        });
        if (clientMessageId) {
          this._postMessageForSession(sid, { type: 'queuedMessageSent', id: clientMessageId });
        }
        return;
      }
    }

    if (rt.isGenerating) {
      rt.queuedMessages.push({ text: trimmed, context, images: attachedImages, clientMessageId, approvalMode });
      return;
    }

    if (clientMessageId) {
      this._postMessageForSession(sid, { type: 'queuedMessageSent', id: clientMessageId });
    }

    if (!attachedImages && await this._handleLocalCommand(trimmed, sid)) {
      return;
    }
    if (!ownerStillBound()) return;

    // A second send can pass the first admission check while this turn awaits
    // local-command handling. Re-check the same session before claiming it.
    if (rt.recoveryLocked) return;
    if (rt.isGenerating) {
      rt.queuedMessages.push({ text: trimmed, context, images: attachedImages, clientMessageId, approvalMode });
      return;
    }

    rt.isGenerating = true;
    rt.terminalSeen = false;
    rt.terminal = undefined;
    rt.streamGeneration = (rt.streamGeneration ?? 0) + 1;
    const turnGeneration = rt.streamGeneration;
    rt.recoveryLocked = false;
    rt.eventBuffer = [];  // Start a fresh buffer for this turn
    rt.eventBuffer.push({ type: 'userMessage', data: { text: trimmed, images: attachedImages } });
    const pending = this._pendingMessages.get(sid);
    if (pending) {
      const currentGeneration = rt.streamGeneration;
      const current = pending.filter((entry) => entry.generation === undefined || entry.generation === currentGeneration);
      if (current.length > 0) this._pendingMessages.set(sid, current);
      else this._pendingMessages.delete(sid);
    }
    this._postStreamEventIfReady(sid, { type: 'generationStarted' });

    let fullMessage = trimmed;
    if (context && context.length > 0) {
      const parts: string[] = [];

      for (const ctx of context) {
        if (ctx.type === 'selection' && ctx.selection) {
          // Use the selected code directly
          const location = ctx.startLine && ctx.endLine
            ? ` (lines ${ctx.startLine}-${ctx.endLine})`
            : '';
          parts.push(`File: ${ctx.fileName || path.basename(ctx.path)}${location}\n\`\`\`${ctx.language || ''}\n${ctx.selection}\n\`\`\``);
        } else {
          // Read entire file (with size limits)
          try {
            const uri = vscode.Uri.file(ctx.path);
            const content = await vscode.workspace.fs.readFile(uri);
            const MAX_FILE_SIZE_BYTES = 512 * 1024;

            if (content.byteLength > MAX_FILE_SIZE_BYTES) {
              parts.push(`File: ${ctx.fileName || path.basename(ctx.path)}\n[File too large to attach (${Math.round(content.byteLength / 1024)} KB). Use a specific selection instead.]`);
              continue;
            }

            const decoded = Buffer.from(content).toString('utf-8');
            const ext = path.extname(ctx.path).slice(1);
            parts.push(`File: ${ctx.fileName || path.basename(ctx.path)}\n\`\`\`${ext}\n${decoded}\n\`\`\``);
          } catch {
            // Skip files that can't be read
          }
        }
      }
      if (parts.length > 0) {
        fullMessage = 'The user has attached the following file(s)/selection(s) for context. The content is provided inline below — DO NOT use read_file to re-read them.\n\n'
          + parts.join('\n\n') + '\n\n' + 'User question: ' + trimmed;
      }
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const request: ChatRequest = {
      message: fullMessage,
      working_dir: workspaceFolder,
      session_id: sid,
      images: attachedImages,
      approval_mode: approvalMode ?? this._approvalModeState.confirmedMode,
    };

    // Capture session ID so callbacks always reference the correct session
    const streamSessionId = sid;
    const streamGeneration = turnGeneration;
    const preparedRuntime = this._sessionRuntimes.get(streamSessionId);
    if (
      preparedRuntime !== rt
      || preparedRuntime.streamGeneration !== streamGeneration
      || !preparedRuntime.isGenerating
      || preparedRuntime.terminalSeen
      || preparedRuntime.recoveryLocked
    ) {
      return;
    }
    const runtimeForStream = () => {
      const current = this._sessionRuntimes.get(streamSessionId);
      return current?.streamGeneration === streamGeneration
        && current.isGenerating
        && !current.terminalSeen
        ? current
        : undefined;
    };

    rt.abortController = this._client.streamChat(request, {
      onRuntimeInfo: (provider, model) => {
        if (!runtimeForStream()) return;
        this.onModelSelected?.(model);
        this._broadcastMessage({ type: 'runtimeInfo', provider, model });
      },
      onMode: (mode) => {
        if (!runtimeForStream()) return;
        if (!this._approvalModeState.pendingMode) {
          this._approvalModeState = initApprovalModeState(mode);
          this._broadcastMessage({ type: 'approvalMode', mode, pending: false });
        }
      },
      onText: (content) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'text', data: { content } });
        this._postStreamEventIfReady(streamSessionId, { type: 'text', content });
      },
      onToolBatch: (calls) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolBatchStart' as const, data: { calls } });
        this._postStreamEventIfReady(streamSessionId, { type: 'toolBatchStart', calls });
      },
      onToolStart: (id, name, args) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolStart', data: { id, name, args } });
        this._postStreamEventIfReady(streamSessionId, { type: 'toolStart', id, name, args });
      },
      onToolProgress: (id, progress) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolProgress', data: { id, progress } });
        this._postStreamEventIfReady(streamSessionId, { type: 'toolProgress', id, progress });
      },
      onToolResult: (id, name, output, success, durationMs) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolResult', data: { id, name, output, success, durationMs } });
        this._postStreamEventIfReady(streamSessionId, { type: 'toolResult', id, name, output, success, durationMs });
      },
      onPermissionRequest: (request) => {
        const srt = runtimeForStream();
        if (!srt) return;
        const msg = {
          sessionId: request.sessionId,
          id: request.callId,
          toolName: request.toolName,
          reason: request.reason,
          args: request.args,
          isDestructive: isDestructivePermissionTool(request.toolName),
        };
        srt.eventBuffer.push({ type: 'permissionRequest', data: msg });
        this._postStreamEventIfReady(streamSessionId, { type: 'permissionRequest', ...msg });
      },
      onTokens: (prompt, completion, total) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'tokens', data: { prompt, completion, total } });
        this._postStreamEventIfReady(streamSessionId, { type: 'tokens', prompt, completion, total });
      },
      onArtifactStart: (id, artifactType, language, title) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'artifactStart', data: { id, artifactType, language, title } });
        this._postStreamEventIfReady(streamSessionId, { type: 'artifactStart', id, artifactType, language, title });
      },
      onArtifactContent: (id, content) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'artifactContent', data: { id, content } });
        this._postStreamEventIfReady(streamSessionId, { type: 'artifactContent', id, content });
      },
      onArtifactEnd: (id) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'artifactEnd', data: { id } });
        this._postStreamEventIfReady(streamSessionId, { type: 'artifactEnd', id });
      },
      onWarning: (message) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'warning', data: { message } });
        this._postStreamEventIfReady(streamSessionId, { type: 'warning', message });
      },
      onPersistenceWarning: (message) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'persistenceWarning', data: { message } });
        this._postStreamEventIfReady(streamSessionId, { type: 'persistenceWarning', message });
      },
      onRateLimited: (event) => {
        const srt = runtimeForStream();
        if (!srt) return;
        srt.eventBuffer.push({ type: 'rateLimited', data: event });
        this._postStreamEventIfReady(streamSessionId, {
          type: 'rateLimited',
          message: event.message,
          retryAfterSeconds: event.retryAfterSeconds,
          attempt: event.attempt,
          maxAttempts: event.maxAttempts,
        });
      },
      onDone: (tokens, toolCalls, sessionId, stopReason, message) => {
        const srt = runtimeForStream();
        if (!srt || srt.terminalSeen) return;
        srt.terminalSeen = true;
        srt.isGenerating = false;
        srt.recoveryLocked = false;
        srt.abortController = undefined;

        if (sessionId && sessionId !== streamSessionId) {
          const wasActive = this._activeSessionId === streamSessionId;
          this._sessionRuntimes.set(sessionId, srt);
          this._sessionRuntimes.delete(streamSessionId);
          // Update panel bindings
          const panel = this._panels.get(streamSessionId);
          this._panels.delete(streamSessionId);
          if (panel) {
            this._panels.set(sessionId, panel);
          }
          const panelReady = this._panelReady.get(streamSessionId);
          this._panelReady.delete(streamSessionId);
          if (panelReady !== undefined) {
            this._panelReady.set(sessionId, panelReady);
          }
          const pending = this._pendingMessages.get(streamSessionId);
          this._pendingMessages.delete(streamSessionId);
          if (pending?.length) {
            const canonicalPending = this._pendingMessages.get(sessionId) || [];
            this._pendingMessages.set(sessionId, [...canonicalPending, ...pending]);
          }
          const info = this._panelSessions.get(streamSessionId);
          if (info) {
            info.sessionId = sessionId;
            this._panelSessions.set(sessionId, info);
            this._panelSessions.delete(streamSessionId);
          }
          if (this._focusedPanelId === streamSessionId) {
            this._focusedPanelId = sessionId;
          }
          if (this._activeSessionId === streamSessionId) {
            this._activeSessionId = sessionId;
          }
          const selection = {
            type: 'sessionSelected',
            sessionId,
            projectHash: srt.projectHash ?? info?.projectHash,
          };
          if (wasActive) {
            this._selectSession(sessionId, selection.projectHash);
          } else {
            this._postOrQueueToPanel(sessionId, selection);
          }
        }

        const doneSessionId = sessionId || streamSessionId;
        srt.terminal = {
          type: 'done',
          generation: streamGeneration,
          tokens,
          toolCalls,
          sessionId,
          stopReason,
          message,
        };
        const completedNormally = !stopReason || stopReason === 'stopped';
        if (!completedNormally) {
          srt.queuedMessages = [];
          this._postTerminalForSession(doneSessionId, { type: 'clearQueuedMessages' }, streamGeneration);
        }
        this._postTerminalForSession(doneSessionId, {
          type: 'done',
          tokens,
          toolCalls,
          sessionId,
          stopReason,
          message,
        }, streamGeneration);
        void this._reloadFinishedSessionHistory(doneSessionId, streamGeneration)
          .finally(() => {
            void this._refreshSessions();
            if (completedNormally) {
              setTimeout(() => void this._sendNextQueuedMessage(doneSessionId), 75);
            }
          });
      },
      onStopped: () => {
        const srt = runtimeForStream();
        if (!srt || srt.terminalSeen) return;
        srt.terminalSeen = true;
        srt.isGenerating = false;
        srt.recoveryLocked = false;
        srt.abortController = undefined;
        srt.queuedMessages = [];
        srt.terminal = { type: 'stopped', generation: streamGeneration };
        this._postTerminalForSession(streamSessionId, { type: 'stopped' }, streamGeneration);
      },
      onError: (message) => {
        const srt = runtimeForStream();
        if (!srt || srt.terminalSeen) return;
        srt.terminalSeen = true;
        srt.isGenerating = false;
        srt.recoveryLocked = true;
        srt.abortController = undefined;
        srt.queuedMessages = [];
        srt.terminal = { type: 'error', generation: streamGeneration, message };
        this._postTerminalForSession(streamSessionId, { type: 'error', message }, streamGeneration);
        this._postTerminalForSession(streamSessionId, { type: 'recoveryRequired' }, streamGeneration);
        void this._reconcileInterruptedSession(streamSessionId, streamGeneration);
      },
    });
  }

  private async _sendNextQueuedMessage(sessionId?: string) {
    if (this._approvalModeState.pendingMode) return;
    const sid = sessionId ?? this._focusedPanelId;
    if (!sid) return;
    const rt = this._sessionRuntimes.get(sid);
    if (!rt || rt.isGenerating) return;
    const next = rt.queuedMessages.shift();
    if (!next) return;
    await this._handleSend(next.text, next.context, next.images, next.clientMessageId, sid, next.approvalMode);
    const rt2 = this._sessionRuntimes.get(sid);
    if (rt2 && !rt2.isGenerating && rt2.queuedMessages.length > 0) {
      void this._sendNextQueuedMessage(sid);
    }
  }

  private _drainReadyQueues() {
    if (this._approvalModeState.pendingMode) return;
    for (const [sessionId, rt] of this._sessionRuntimes) {
      if (!rt.isGenerating && rt.queuedMessages.length > 0) {
        void this._sendNextQueuedMessage(sessionId);
      }
    }
  }

  private async _reconcileInterruptedSession(sessionId: string, generation: number) {
    try {
      const activeSessions = await this._client.activeSessions();
      const rt = this._sessionRuntimes.get(sessionId);
      if (!rt || rt.streamGeneration !== generation || !rt.recoveryLocked) return;
      if (!activeSessions.includes(sessionId)) {
        rt.recoveryLocked = false;
        this._postTerminalForSession(sessionId, { type: 'recoveryCleared' }, generation);
      }
    } catch {
      // Keep the recovery lock when daemon truth cannot be read. A later send
      // re-checks /chat/active and refuses to overlap an unknown live turn.
    }
  }

  /// 重开窗口后，daemon 侧旧回合仍在运行但 VSCode 已无法 reattach 流。
  /// 通过轮询 /chat/active 检测旧回合是否结束：
  ///   - 已结束 → 重置 isGenerating，触发队列重放
  ///   - 仍在运行 → 保持 isGenerating = true，新消息走入队路径
  /// 首次轮询 500ms（重开窗口后快速检测），后续退避至 2s。
  /// 超时（5 分钟）后不强制重置，保持状态由用户决定后续操作。
  private async _pollActiveSessionRecovery(sessionId: string, generation: number): Promise<void> {
    // 去重：若已有进行中的轮询，不重复启动
    const existing = this._sessionRuntimes.get(sessionId);
    if (existing?.pollHandle && !existing.pollHandle.cancelled) return;

    const handle = { cancelled: false };
    const rt0 = this._sessionRuntimes.get(sessionId);
    if (rt0) rt0.pollHandle = handle;

    const FIRST_POLL_MS = 500;
    const POLL_INTERVAL_MS = 2000;
    const MAX_POLLS = 150; // 首次 500ms + 149 次 * 2s ≈ 5 分钟

    for (let i = 0; i < MAX_POLLS; i++) {
      await delay(i === 0 ? FIRST_POLL_MS : POLL_INTERVAL_MS);
      if (handle.cancelled) return;

      const rt = this._sessionRuntimes.get(sessionId);
      if (!rt || rt.streamGeneration !== generation) return; // 已被新回合取代
      if (!rt.isGenerating) return; // 已被其他路径重置

      try {
        const activeSessions = await this._client.activeSessions();
        if (handle.cancelled) return;
        if (!activeSessions.includes(sessionId)) {
          // 旧回合已结束 → 重置状态，触发队列重放
          rt.isGenerating = false;
          rt.recoveryLocked = false;
          rt.terminal = undefined;
          rt.pollHandle = undefined;
          this._postMessageForSession(sessionId, { type: 'generationStopped' });
          void this._sendNextQueuedMessage(sessionId);
          return;
        }
        // 仍在运行 → 继续轮询
      } catch {
        // daemon 不可达 → 继续轮询，保持 isGenerating = true
      }
    }

    // 超时：不强制重置，保持 isGenerating = true 由用户决定后续操作
    const rtFinal = this._sessionRuntimes.get(sessionId);
    if (rtFinal && rtFinal.streamGeneration === generation && rtFinal.pollHandle === handle) {
      rtFinal.pollHandle = undefined;
    }
  }

  private async _handlePermissionResponse(msg: {
    sessionId?: string;
    id?: string;
    toolName?: string;
    decision?: unknown;
    allowed?: boolean;
    persist?: boolean;
  }) {
    if (!msg.sessionId || !msg.id) return;
    const decision: PermissionDecision | undefined = isPermissionDecision(msg.decision)
      ? msg.decision
      : typeof msg.allowed === 'boolean'
        ? (msg.allowed ? (msg.persist ? 'allow_persist' : 'allow') : 'deny')
        : undefined;

    if (!decision) {
      this._postMessageForSession(msg.sessionId, {
        type: 'permissionResponseResult',
        id: msg.id,
        success: false,
        message: 'Invalid permission decision',
      });
      return;
    }

    try {
      const result = await this._client.sendPermissionDecision(
        msg.sessionId,
        decision,
        msg.toolName,
      );
      this._postMessageForSession(msg.sessionId, {
        type: 'permissionResponseResult',
        id: msg.id,
        success: result.success,
        message: result.error,
      });
    } catch (e) {
      this._postMessageForSession(msg.sessionId, {
        type: 'permissionResponseResult',
        id: msg.id,
        success: false,
        message: this._messageFromError(e),
      });
    }
  }

  private async _reloadFinishedSessionHistory(sessionId: string, generation: number) {
    const info = this._panelSessions.get(sessionId);
    const projectHash = info?.projectHash ?? this._sessionRuntimes.get(sessionId)?.projectHash;
    if (!projectHash) return;

    try {
      const detail = await this._client.getSession(projectHash, sessionId);
      const messages = detail?.messages;
      if (!messages) return;
      const rt = this._sessionRuntimes.get(sessionId);
      if (!rt || rt.streamGeneration !== generation) return;

      const currentInfo = this._panelSessions.get(sessionId);
      if (currentInfo && this._panels.has(sessionId)) {
        this._panelSessions.set(sessionId, {
          ...currentInfo,
          sessionId,
          projectHash,
          messages,
          messagesPromise: undefined,
        });
      }
      rt.messages = messages;
      rt.eventBuffer = [];
      this._postOrQueueToPanel(sessionId, {
        type: 'sessionMessages',
        messages,
        terminal: this._terminalForWebview(rt.terminal),
      }, generation);
    } catch {
      // The already-delivered stream remains visible if history refresh fails.
    }
  }

  private async _ensureSession(forPanelSessionId?: string): Promise<string | undefined> {
    const sid = forPanelSessionId ?? this._focusedPanelId;
    if (sid) {
      const rt = this._getRuntime(sid);
      rt.projectHash ??= this._panelSessions.get(sid)?.projectHash;
      return sid;
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const session = await this._client.createSession(undefined, workspaceFolder);
    this._getRuntime(session.id).projectHash = session.project_hash;

    await this._refreshSessions();
    return session.id;
  }


  public sendEditorContext() {
    this._sendEditorContext();
  }

  // New protocol methods

  private async _sendInitialState(
    webview?: vscode.Webview,
    mode: WebviewMode = 'tab',
  ): Promise<StreamReplayCursor> {
    let currentModelName = '';
    let sid = webview && mode === 'tab' ? this._sessionIdForWebview(webview) : undefined;
    const initialRuntime = sid ? this._getRuntime(sid) : undefined;
    const initialGeneration = initialRuntime?.streamGeneration ?? 0;
    let messagesToLoad: MessageInfo[] | undefined;
    let streamGeneration = 0;
    let replayedEvents = 0;
    let historyGeneration: number | undefined;
    let terminalGeneration: number | undefined;

    if (sid) {
      const info = this._panelSessions.get(sid);
      messagesToLoad = info?.messages ?? initialRuntime?.messages;
      if (!messagesToLoad && info?.messagesPromise) {
        try {
          messagesToLoad = await info.messagesPromise;
          if (messagesToLoad) {
            info.messages = messagesToLoad;
            initialRuntime!.messages = messagesToLoad;
          }
        } catch {
          // The remaining initial state still renders without history.
        }
      }
    }

    await this._sendSetupState(webview);

    try {
      const models = await this._client.listModels();
      this._postMessage({ type: 'models', models }, webview);
      const defaultModel = models.find((m: { is_default: boolean }) => m.is_default);
      if (defaultModel) {
        currentModelName = (defaultModel as { model: string }).model || '';
      }
    } catch { /* daemon not available */ }

    let currentProjectHash: string | undefined;
    try {
      const loaded = await this._loadSessionsForDisplay();
      const sessions = loaded.sessions;
      currentProjectHash = loaded.currentProjectHash;
      await this._annotateSessionGenerating(sessions as any[]);
      this._postMessage({ type: 'sessions', sessions }, webview);
    } catch {}

    try {
      const modeResp = await this._client.getApprovalMode();
      if (!this._approvalModeState.pendingMode) {
        this._approvalModeState = initApprovalModeState(modeResp.mode);
        this._postMessage({ type: 'approvalMode', mode: modeResp.mode, pending: false }, webview);
      } else {
        this._postMessage({
          type: 'approvalMode',
          mode: this._approvalModeState.displayMode,
          pending: true,
        }, webview);
      }
    } catch {}

    this._sendEditorContext(webview);

    if (webview && mode === 'tab') {
      sid = this._sessionIdForWebview(webview);
    }

    if (sid) {
      const beforeActiveCheck = this._getRuntime(sid);
      if (!beforeActiveCheck.isGenerating && !beforeActiveCheck.abortController) {
        try {
          const activeSessions = await this._client.activeSessions();
          if (activeSessions.includes(sid)) {
            // 重开窗口检测到 daemon 侧仍有活跃回合：
            // 标记 isGenerating = true 使后续 _handleSend 走正常入队路径，
            // 并启动轮询以在旧回合结束后触发队列重放。
            beforeActiveCheck.isGenerating = true;
            beforeActiveCheck.recoveryLocked = false;
            beforeActiveCheck.terminal = undefined;
            this._postMessageForSession(sid, { type: 'generationStarted' });
            void this._pollActiveSessionRecovery(sid, beforeActiveCheck.streamGeneration ?? 0);
          } else if (beforeActiveCheck.recoveryLocked) {
            beforeActiveCheck.recoveryLocked = false;
          }
        } catch {
          // If daemon truth is unavailable, preserve any existing recovery lock.
        }
      }

      const rt = this._getRuntime(sid);
      streamGeneration = rt.streamGeneration ?? 0;
      const historyMatchesGeneration = streamGeneration === initialGeneration;
      const terminal = rt.terminal?.generation === streamGeneration ? rt.terminal : undefined;

      if (messagesToLoad && mode === 'tab' && historyMatchesGeneration) {
        this._postMessage({
          type: 'sessionMessages',
          messages: messagesToLoad,
          terminal: this._terminalForWebview(terminal),
        }, webview);
        historyGeneration = streamGeneration;
        if (terminal) terminalGeneration = terminal.generation;
      }

      replayedEvents = this._replayStreamBuffer(sid, rt, webview);
      if (!messagesToLoad && terminal) {
        this._postMessage(this._terminalForWebview(terminal), webview);
        terminalGeneration = terminal.generation;
      }
    }

    const projectHash = sid
      ? (this._panelSessions.get(sid)?.projectHash ?? this._sessionRuntimes.get(sid)?.projectHash ?? currentProjectHash)
      : currentProjectHash;

    this._postMessage({
      type: 'init',
      generating: sid
        ? Boolean(this._sessionRuntimes.get(sid)?.isGenerating || this._sessionRuntimes.get(sid)?.recoveryLocked)
        : false,
      recoveryLocked: sid ? Boolean(this._sessionRuntimes.get(sid)?.recoveryLocked) : false,
      currentModel: currentModelName,
      viewMode: mode,
      activeSessionId: sid,
      projectHash,
      isSessionList: mode === 'sidebar',
      locale: vscode.env.language,
      approvalMode: this._approvalModeState.displayMode,
      approvalModePending: Boolean(this._approvalModeState.pendingMode),
    }, webview);
    return { sessionId: sid, streamGeneration, replayedEvents, historyGeneration, terminalGeneration };
  }

  private async _sendSetupState(webview?: vscode.Webview) {
    const generation = ++this._setupStateGeneration;
    const isCurrent = () => generation === this._setupStateGeneration;
    let auth: AuthStatusResponse | undefined;
    let providers: ProvidersResponse | undefined;
    let config: ConfigResponse | undefined;
    let models: ModelInfo[] | undefined;
    const post = (msg: unknown) => {
      if (!isCurrent()) return;
      // Setup state is process-global. Broadcasting the newest snapshot keeps
      // every open view coherent when a panel-focused refresh supersedes an
      // older watcher refresh. A not-yet-registered webview still receives its
      // direct initialization copy.
      this._broadcastMessage(msg);
      const tracked = webview && (
        this._view?.webview === webview
        || this._webviewPanels.has(webview)
      );
      if (webview && !tracked) this._postMessage(msg, webview);
    };

    try {
      auth = await this._client.authStatus();
      if (!isCurrent()) return;
      post({ type: 'authStatus', auth });
      this._watchAtomCodeAuth(auth.auth_path);
    } catch (e) {
      if (!isCurrent()) return;
      post({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      providers = await this._client.listProviders();
      if (!isCurrent()) return;
      post({ type: 'providers', providers: providers.providers, defaultProvider: providers.default_provider });
    } catch (e) {
      if (!isCurrent()) return;
      post({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      config = await this._client.getConfig();
      if (!isCurrent()) return;
      post({ type: 'config', config });
      this._watchAtomCodeConfig(config.path);
    } catch {
      if (!isCurrent()) return;
      // Older daemons may not have P0 APIs; provider fetch error already surfaces enough.
    }

    try {
      models = await this._client.listModels();
      if (!isCurrent()) return;
      post({ type: 'models', models });
    } catch {
      if (!isCurrent()) return;
    }

    const defaultProvider = providers?.providers.find((p) => p.is_default);
    const authUnavailable = !auth?.logged_in || auth.expired === true;
    const selectedRequiresLogin = defaultProvider?.requires_login;
    post({
      type: 'setupState',
      auth,
      providers: providers?.providers ?? [],
      defaultProvider: providers?.default_provider ?? config?.default_provider ?? '',
      currentModel: defaultProvider?.model || models?.find((m) => m.is_default)?.model || '',
      setupRequired: (providers?.providers.length ?? 0) === 0
        || (selectedRequiresLogin === undefined
          ? authUnavailable
          : selectedRequiresLogin && authUnavailable),
    });
  }

  private _watchAtomCodeConfig(configPath: string) {
    if (!configPath || this._watchedConfigPath === configPath) return;
    this._atomCodeConfigWatcher?.dispose();
    this._watchedConfigPath = configPath;
    const watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(path.dirname(configPath), path.basename(configPath)),
    );
    const scheduleRefresh = () => this._scheduleSetupStateRefresh();
    watcher.onDidCreate(scheduleRefresh);
    watcher.onDidChange(scheduleRefresh);
    watcher.onDidDelete(scheduleRefresh);
    this._atomCodeConfigWatcher = watcher;
  }

  private _watchAtomCodeAuth(authPath: string) {
    if (!authPath || this._watchedAuthPath === authPath) return;
    this._atomCodeAuthWatcher?.dispose();
    this._watchedAuthPath = authPath;
    const watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(path.dirname(authPath), path.basename(authPath)),
    );
    const scheduleRefresh = () => this._scheduleSetupStateRefresh();
    watcher.onDidCreate(scheduleRefresh);
    watcher.onDidChange(scheduleRefresh);
    watcher.onDidDelete(scheduleRefresh);
    this._atomCodeAuthWatcher = watcher;
  }

  private _scheduleSetupStateRefresh() {
    if (this._setupRefreshTimer) clearTimeout(this._setupRefreshTimer);
    this._setupRefreshTimer = setTimeout(() => {
      this._setupRefreshTimer = undefined;
      void this._sendSetupState();
    }, 100);
  }

  private async _startLogin() {
    if (this._loginInFlight) return;
    this._loginInFlight = true;
    try {
      await this._cancelLogin();
      const login = await this._client.startLogin(true);
      this._loginId = login.login_id;
      const generation = ++this._loginGeneration;
      this._broadcastMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });
      await this._pollLogin(login.login_id, generation, login.expires_in_seconds);
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
    } finally {
      this._loginInFlight = false;
    }
  }

  private async _pollLogin(loginId: string, generation: number, expiresInSeconds: number) {
    const deadline = Date.now() + Math.max(1, expiresInSeconds) * 1000;
    try {
      while (this._loginId === loginId && this._loginGeneration === generation) {
        if (Date.now() >= deadline) {
          await this._client.cancelLogin(loginId).catch(() => undefined);
          throw new Error('Login timed out; start a new login.');
        }

        let result;
        try {
          result = await this._client.pollLogin(loginId);
        } catch (error) {
          if (error instanceof DaemonHttpError && error.retryable && Date.now() < deadline) {
            this._broadcastMessage({ type: 'loginPending' });
            await delay(2000);
            continue;
          }
          throw error;
        }

        if (result.status === 'pending') {
          this._broadcastMessage({ type: 'loginPending' });
          await delay(result.retry_after_ms ?? 2000);
          continue;
        }
        if (result.status !== 'authorized') {
          throw new Error(result.message || `Login ${result.status}; start a new login.`);
        }

        if (this._loginId !== loginId || this._loginGeneration !== generation) return;
        this._loginId = undefined;
        this._broadcastMessage({ type: 'loginAuthorized', user: result.user });
        if (this._loginStartedFromCommand) {
          this._postMessage({
            type: 'assistantMessage',
            text: vscode.l10n.t('Signed in as {name}.', { name: result.user?.name || result.user?.username || vscode.l10n.t('AtomGit user') }),
          });
          this._loginStartedFromCommand = false;
        }
        await this._sendSetupState();
        return;
      }
    } catch (e) {
      if (this._loginGeneration !== generation) return;
      this._loginGeneration += 1;
      this._loginId = undefined;
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
      if (this._loginStartedFromCommand) {
        this._postMessage({ type: 'error', message: this._messageFromError(e) });
        this._loginStartedFromCommand = false;
      }
    }
  }

  private async _cancelLogin() {
    this._loginGeneration += 1;
    if (this._loginId) {
      const id = this._loginId;
      this._loginId = undefined;
      await this._client.cancelLogin(id).catch(() => undefined);
    }
  }

  private async _ensureLoggedInForCodingPlan(announceInChat = false): Promise<boolean> {
    let ownsLogin = false;
    try {
      const auth = await this._client.authStatus();
      if (auth.logged_in && !auth.expired) {
        return true;
      }
      if (this._loginInFlight) {
        while (this._loginInFlight) await delay(100);
        const refreshed = await this._client.authStatus();
        return refreshed.logged_in && !refreshed.expired;
      }
      this._loginInFlight = true;
      ownsLogin = true;

      if (announceInChat) {
        this._postMessage({
          type: 'assistantMessage',
          text: vscode.l10n.t('Opening AtomGit sign-in in your browser. Complete authorization there, then return to VS Code.'),
        });
      }
      this._broadcastMessage({ type: 'setupWorking', message: vscode.l10n.t('Waiting for AtomGit sign-in...') });

      await this._cancelLogin();
      const login = await this._client.startLogin(true);
      this._loginId = login.login_id;
      const generation = ++this._loginGeneration;
      const deadline = Date.now() + Math.max(1, login.expires_in_seconds) * 1000;
      this._broadcastMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });

      while (this._loginId === login.login_id && this._loginGeneration === generation) {
        if (Date.now() >= deadline) {
          await this._cancelLogin();
          throw new Error('Login timed out; start a new login.');
        }
        let result;
        try {
          result = await this._client.pollLogin(login.login_id);
        } catch (error) {
          if (error instanceof DaemonHttpError && error.retryable && Date.now() < deadline) {
            await delay(2000);
            continue;
          }
          throw error;
        }
        if (result.status === 'pending') {
          this._broadcastMessage({ type: 'loginPending' });
          await delay(result.retry_after_ms ?? 2000);
          continue;
        }
        if (result.status !== 'authorized') {
          throw new Error(result.message || `Login ${result.status}; start a new login.`);
        }

        this._loginId = undefined;
        this._broadcastMessage({ type: 'loginAuthorized', user: result.user });
        if (announceInChat) {
          this._postMessage({
            type: 'assistantMessage',
            text: vscode.l10n.t('Signed in as {name}.', { name: result.user?.name || result.user?.username || vscode.l10n.t('AtomGit user') }),
          });
        }
        await this._sendSetupState();
        return true;
      }

      return false;
    } catch (e) {
      this._loginGeneration += 1;
      this._loginId = undefined;
      const message = this._messageFromError(e);
      this._broadcastMessage({ type: 'setupError', message });
      if (announceInChat) {
        this._postMessage({ type: 'error', message });
      }
      return false;
    } finally {
      if (ownsLogin) this._loginInFlight = false;
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
          text: vscode.l10n.t('Syncing CodingPlan models...'),
        });
      }
      this._broadcastMessage({ type: 'setupWorking', message: vscode.l10n.t('Syncing CodingPlan models...') });
      const result: CodingPlanSetupResponse = await this._client.setupCodingPlan(this._loginId);
      this._broadcastMessage({ type: 'codingPlanResult', result });
      await this._sendSetupState();
      return result;
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
      return undefined;
    }
  }

  private async _createProvider(provider: CreateProviderRequest) {
    try {
      await this._client.createProvider(provider);
      await this._sendSetupState();
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
      await this._sendSetupState();
    }
  }

  private async _deleteProvider(name: string) {
    try {
      await this._client.deleteProvider(name);
      await this._sendSetupState();
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
      // The webview updates optimistically; re-read daemon truth on failure.
      await this._sendSetupState();
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
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
      // The webview updates optimistically; re-read daemon truth on failure.
      await this._sendSetupState();
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

  private async _setReasoningEffort(provider: string, effort: string | null) {
    if (!provider) return;
    try {
      await this._client.setReasoningEffort(provider, effort);
      const models = await this._client.listModels();
      this._broadcastMessage({ type: 'models', models });
    } catch {
      // Re-fetch current models to revert the optimistic UI update
      try {
        const models = await this._client.listModels();
        this._broadcastMessage({ type: 'models', models });
      } catch {
        // Recovery failed — silently ignore
      }
    }
  }

  private async _setApprovalMode(mode: ApprovalMode) {
    const next = beginApprovalModeSwitch(this._approvalModeState, mode);
    if (next === this._approvalModeState) return;
    this._approvalModeState = next;
    this._broadcastMessage({ type: 'approvalMode', mode: next.displayMode, pending: true });
    try {
      const resp = await this._client.setApprovalMode(mode);
      this._approvalModeState = completeApprovalModeSwitch(this._approvalModeState, resp.mode);
      this._broadcastMessage({
        type: 'approvalMode',
        mode: this._approvalModeState.displayMode,
        pending: false,
      });
    } catch {
      this._approvalModeState = failApprovalModeSwitch(this._approvalModeState);
      this._broadcastMessage({
        type: 'approvalMode',
        mode: this._approvalModeState.displayMode,
        pending: false,
      });
    } finally {
      this._drainReadyQueues();
    }
  }

  private _sendEditorContext(webview?: vscode.Webview) {
    // Only send context when user has an active selection
    const editor = vscode.window.activeTextEditor;
    if (editor && !editor.selection.isEmpty) {
      const selection = editor.selection;
      this._postMessage({
        type: 'context',
        filePath: editor.document.uri.fsPath,
        fileName: path.basename(editor.document.uri.fsPath),
        selection: editor.document.getText(selection),
        language: editor.document.languageId,
        startLine: selection.start.line + 1,
        endLine: selection.end.line + 1,
      }, webview);
    }
  }

  public async openSessionInTab(sessionId?: string, projectHash?: string) {
    if (!sessionId) {
      await this.newConversation();
      return;
    }

    const existing = this._panels.get(sessionId);
    if (existing) {
      const cached = this._panelSessions.get(sessionId);
      let hash = projectHash
        ?? cached?.projectHash
        ?? this._sessionRuntimes.get(sessionId)?.projectHash;
      const cachedForProject = !projectHash || cached?.projectHash === projectHash
        ? cached
        : undefined;
      let messages = cachedForProject?.messages;

      if (messages === undefined) {
        hash ??= await this._resolveSessionProjectHash(sessionId);
        if (this._panels.get(sessionId) !== existing) return;
        if (!hash) {
          vscode.window.showErrorMessage(vscode.l10n.t('Unable to open session: missing project hash.'));
          return;
        }
        try {
          const pendingForProject = cachedForProject?.messagesPromise;
          messages = await (pendingForProject
            ?? this._loadPanelHistory(existing, sessionId, hash));
          if (this._panels.get(sessionId) !== existing) return;
          if (this._panelSessions.get(sessionId)?.projectHash !== hash) return;
          if (messages === undefined) {
            existing.reveal();
            this._focusedPanelId = sessionId;
            this._activeSessionId = sessionId;
            this._selectSession(sessionId, hash);
            return;
          }
        } catch (e) {
          if (this._panels.get(sessionId) !== existing) return;
          if (this._panelSessions.get(sessionId)?.projectHash !== hash) return;
          vscode.window.showErrorMessage(`Unable to load session: ${this._messageFromError(e)}`);
          return;
        }
      }

      existing.reveal();
      this._focusedPanelId = sessionId;
      this._activeSessionId = sessionId;
      this._selectSession(sessionId, hash);
      return;
    }

    const hash = await this._resolveSessionProjectHash(sessionId, projectHash);

    if (!hash) {
      vscode.window.showErrorMessage(vscode.l10n.t('Unable to open session: missing project hash.'));
      return;
    }

    let messages: MessageInfo[] | undefined;
    try {
      const detail = await this._client.getSession(hash, sessionId);
      messages = detail?.messages;
    } catch (e) {
      vscode.window.showErrorMessage(`Unable to load session: ${this._messageFromError(e)}`);
      return;
    }

    this._panelSessions.set(sessionId, { sessionId, projectHash: hash, messages });
    const rt = this._getRuntime(sessionId);
    rt.projectHash = hash;
    rt.messages = messages;
    this._activeSessionId = sessionId;
    this.openInTab(sessionId);
    this._selectSession(sessionId, hash);
    await this._refreshSessions();
  }

  // Replay buffered stream events so the webview can resume displaying a
  // background stream. `fromIndex` is used by the ready-handshake catch-up pass:
  // events arriving across the final await are replayed once before live
  // forwarding is enabled.
  private _replayStreamBuffer(
    _sessionId: string,
    rt: SessionRuntime,
    webview?: vscode.Webview,
    fromIndex = 0,
  ): number {
    if ((!rt.isGenerating && !rt.terminal) || rt.eventBuffer.length === 0) return 0;

    const post = (msg: unknown) => webview
      ? webview.postMessage(msg)
      : this._broadcastMessage(msg);

    const replayedEvents = rt.eventBuffer.length;
    const snapshot = rt.eventBuffer.slice(fromIndex, replayedEvents);

    if (fromIndex === 0) {
      for (const evt of snapshot) {
        if (evt.type === 'userMessage') {
          post({ type: 'userMessage', text: evt.data.text });
        }
      }
      post({ type: 'resumeStreaming' });
    }

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
        case 'toolProgress':
          post({ type: 'toolProgress', id: evt.data.id, progress: evt.data.progress });
          break;
        case 'toolResult':
          post({ type: 'toolResult', id: evt.data.id, name: evt.data.name, output: evt.data.output, success: evt.data.success, durationMs: evt.data.durationMs });
          break;
        case 'permissionRequest':
          post({ type: 'permissionRequest', ...evt.data });
          break;
        case 'artifactStart':
          post({ type: 'artifactStart', id: evt.data.id, artifactType: evt.data.artifactType, language: evt.data.language, title: evt.data.title });
          break;
        case 'artifactContent':
          post({ type: 'artifactContent', id: evt.data.id, content: evt.data.content });
          break;
        case 'artifactEnd':
          post({ type: 'artifactEnd', id: evt.data.id });
          break;
        case 'warning':
          post({ type: 'warning', message: evt.data.message });
          break;
        case 'persistenceWarning':
          post({ type: 'persistenceWarning', message: evt.data.message });
          break;
        case 'rateLimited':
          post({
            type: 'rateLimited',
            message: evt.data.message,
            retryAfterSeconds: evt.data.retryAfterSeconds,
            attempt: evt.data.attempt,
            maxAttempts: evt.data.maxAttempts,
          });
          break;
        case 'tokens':
          post({ type: 'tokens', prompt: evt.data.prompt, completion: evt.data.completion, total: evt.data.total });
          break;
      }
    }
    return replayedEvents;
  }

  private async _renameSession(sessionId: string, projectHash?: string, currentName?: string) {
    const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
    if (!hash) {
      this._postMessage({ type: 'error', message: vscode.l10n.t('Unable to rename session: missing project hash.') });
      return;
    }

    const nextName = await vscode.window.showInputBox({
      title: vscode.l10n.t('Rename AtomCode session'),
      prompt: vscode.l10n.t('Enter a new session name'),
      value: currentName || '',
      ignoreFocusOut: true,
      validateInput: (value) => value.trim() ? undefined : vscode.l10n.t('Session name cannot be empty'),
    });
    if (nextName === undefined) return;

    try {
      await this._client.renameSession(hash, sessionId, nextName.trim());
      await this._refreshSessions();
    } catch (e) {
      this._postMessage({ type: 'error', message: vscode.l10n.t('Unable to rename session: {message}', { message: this._messageFromError(e) }) });
    }
  }

  private async _deleteSession(sessionId: string, projectHash?: string, currentName?: string) {
    const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
    if (!hash) {
      this._postMessage({ type: 'error', message: vscode.l10n.t('Unable to delete session: missing project hash.') });
      return;
    }

    const label = currentName || sessionId;
    const deleteLabel = vscode.l10n.t('Delete');
    const choice = await vscode.window.showWarningMessage(
      vscode.l10n.t('Delete AtomCode session "{label}"?', { label }),
      { modal: true, detail: vscode.l10n.t('This removes the session from local history.') },
      deleteLabel,
    );
    if (choice !== deleteLabel) return;

    try {
      await this._deleteSessionInternal(sessionId, hash);
    } catch (e) {
      this._postMessage({ type: 'error', message: vscode.l10n.t('Unable to delete session: {message}', { message: this._messageFromError(e) }) });
    }
  }

  private async _deleteSessionInternal(sessionId: string, hash: string) {
    const deletingActiveSession = this._activeSessionId === sessionId;
    // Stop any daemon-side stream before deleting
    const rt = this._sessionRuntimes.get(sessionId);
    if (rt?.isGenerating) {
      rt.abortController?.abort();
      void this._client.stopGeneration(sessionId).catch(() => undefined);
    }
    // Delete on the server side first; only clear local state after success.
    // This avoids data loss when the HTTP call fails (e.g. network / daemon crash).
    await this._client.deleteSession(hash, sessionId);

    this._sessionRuntimes.delete(sessionId);

    // Clear any panels bound to this session
    for (const [pid, info] of [...this._panelSessions]) {
      if (info.sessionId === sessionId) {
        this._panelSessions.delete(pid);
        if (this._focusedPanelId === pid) {
          this._focusedPanelId = undefined;
        }
      }
    }
    // Also close and clear direct panel binding (tab opened via openSessionInTab)
    const panel = this._panels.get(sessionId);
    if (panel) {
      this._panels.delete(sessionId);
      this._panelReady.delete(sessionId);
      this._panelSessions.delete(sessionId);
      if (this._focusedPanelId === sessionId) {
        this._focusedPanelId = undefined;
      }
      panel.dispose();
    }
    if (deletingActiveSession) {
      this._selectSession(undefined, undefined);
      this._view?.webview.postMessage({ type: 'clearChat' });
    }
    await this._refreshSessions();
  }

  private async _deleteSessions(
    sessions: Array<{ sessionId: string; projectHash?: string; name?: string }>,
    sourceWebview?: vscode.Webview,
  ) {
    if (!sessions || sessions.length === 0) return;

    const count = sessions.length;
    const label = count === 1
      ? vscode.l10n.t('Delete AtomCode session "{label}"?', { label: sessions[0].name || sessions[0].sessionId })
      : vscode.l10n.t('Delete {count} sessions?', { count });
    const deleteLabel = vscode.l10n.t('Delete');

    const choice = await vscode.window.showWarningMessage(
      label,
      { modal: true, detail: vscode.l10n.t('This cannot be undone. Sessions will be removed from local history.') },
      deleteLabel,
    );
    if (choice !== deleteLabel) return;

    let succeeded = 0;
    let failed = 0;
    for (const { sessionId, projectHash, name } of sessions) {
      const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
      if (!hash) {
        // No daemon record — clean up panel bindings locally
        const deletingActiveSession = this._activeSessionId === sessionId;
        for (const [pid, info] of [...this._panelSessions]) {
          if (info.sessionId === sessionId) {
            this._panelSessions.delete(pid);
            if (this._focusedPanelId === pid) {
              this._focusedPanelId = undefined;
            }
          }
        }
        const panel = this._panels.get(sessionId);
        if (panel) {
          this._panels.delete(sessionId);
          this._panelReady.delete(sessionId);
          if (this._focusedPanelId === sessionId) {
            this._focusedPanelId = undefined;
          }
          panel.dispose();
        }
        if (deletingActiveSession) {
          this._selectSession(undefined, undefined);
          this._view?.webview.postMessage({ type: 'clearChat' });
        }
        succeeded++;
        continue;
      }
      try {
        await this._deleteSessionInternal(sessionId, hash);
        succeeded++;
      } catch (e) {
        failed++;
      }
    }

    if (failed > 0) {
      const errMsg = { type: 'error', message: vscode.l10n.t('Deleted {succeeded}/{count} sessions, {failed} failed', { succeeded, count, failed }) };
      if (sourceWebview) {
        this._postMessage(errMsg, sourceWebview);
      } else {
        this._postMessage(errMsg);
      }
    }

    await this._refreshSessions();
  }

  private async _resolveSessionProjectHash(sessionId: string, projectHash?: string): Promise<string | undefined> {
    if (projectHash) return projectHash;
    const boundProjectHash = this._panelSessions.get(sessionId)?.projectHash
      ?? this._sessionRuntimes.get(sessionId)?.projectHash;
    if (boundProjectHash) return boundProjectHash;
    try {
      return (await this._client.resolveSession(sessionId)).project_hash;
    } catch {
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (!workspaceFolder) return undefined;
      try {
        const scoped = await this._client.listSessionsForWorkingDir(workspaceFolder);
        return scoped.find((session) => session.id === sessionId)?.project_hash;
      } catch {
        try {
          const resolvedWorkspace = path.resolve(workspaceFolder);
          const matches = (await this._client.listSessions()).filter((session) =>
            session.id === sessionId
            && Boolean(session.project_hash)
            && (session.working_dir
              ? path.resolve(session.working_dir) === resolvedWorkspace
              : false)
          );
          const projectHashes = new Set(matches.map((session) => session.project_hash));
          return projectHashes.size === 1 ? matches[0]?.project_hash : undefined;
        } catch {
          return undefined;
        }
      }
    }
  }

  private async _applyCode(code: string, _language: string) {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showInformationMessage(vscode.l10n.t('No active editor to apply code to'));
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

  private async _handleQuickAction(
    action: string,
    targetSessionId?: string,
    ownerWebview?: vscode.Webview,
  ) {
    const prompt = getQuickActionPrompt(action, vscode.env.language);
    const sid = targetSessionId ?? this._focusedPanelId ?? await this._ensureSession();
    if (!sid) return;
    if (ownerWebview && this._sessionIdForWebview(ownerWebview) !== sid) return;
    this._postOrQueueToPanel(sid, { type: 'userMessage', text: prompt });
    await this._handleSend(prompt, undefined, undefined, undefined, sid, undefined, ownerWebview);
  }

  private async _handleSlashCommand(
    command: string,
    targetSessionId?: string,
    ownerWebview?: vscode.Webview,
  ) {
    const sid = targetSessionId ?? this._focusedPanelId ?? await this._ensureSession();
    if (!sid) return;
    if (ownerWebview && this._sessionIdForWebview(ownerWebview) !== sid) return;
    await this._handleLocalCommand(command.trim(), sid);
  }

  private async _handleLocalCommand(text: string, sessionId?: string): Promise<boolean> {
    const [command] = text.split(/\s+/, 1);
    switch (command.toLowerCase()) {
      case '/login':
        {
          const result = await this._setupCodingPlan({ loginIfNeeded: true, announceInChat: true });
          if (result) {
            this._postSlashInfo('```\n' + result.report_text + '\n```', sessionId, text);
          }
        }
        return true;
      case '/logout':
        try {
          const auth = await this._client.logout();
          this._broadcastMessage({ type: 'authStatus', auth });
          this._postSlashInfo(vscode.l10n.t('Signed out of AtomGit.'), sessionId, text);
        } catch (e) {
          this._postSlashInfo(vscode.l10n.t('Unable to sign out: {message}', { message: this._messageFromError(e) }), sessionId, text);
        }
        return true;
      case '/whoami':
        try {
          const auth = await this._client.authStatus();
          const authState = classifyAuthDisplayState(auth);
          if (authState === 'expired') {
            this._postSlashInfo(vscode.l10n.t('AtomGit session expired. Sign in again.'), sessionId, text);
          } else if (authState === 'signed_in' && auth.user) {
            const name = auth.user.name || auth.user.username || auth.user.email || auth.user.id;
            const lines = [
              `${name} (${auth.user.username || auth.user.id})`,
              auth.user.email || vscode.l10n.t('Email: not provided'),
              `User ID: ${auth.user.id}`,
              `Auth: ${auth.auth_path}`,
            ];
            if (auth.token) {
              lines.push(`Token: ${auth.token.token_type}`);
              lines.push(`Created: ${new Date(auth.token.created_at * 1000).toLocaleString()}`);
              if (auth.token.expires_in !== undefined) {
                lines.push(`Expires in: ${auth.token.expires_in}s`);
              }
              lines.push(`Refresh token: ${auth.token.has_refresh_token ? vscode.l10n.t('yes') : vscode.l10n.t('no')}`);
            }
            this._postSlashInfo(lines.join('\n'), sessionId, text);
          } else {
            this._postSlashInfo(vscode.l10n.t('Not signed in.'), sessionId, text);
          }
        } catch (e) {
          this._postSlashInfo(vscode.l10n.t('Unable to read auth status: {message}', { message: this._messageFromError(e) }), sessionId, text);
        }
        return true;
      case '/status':
        try {
          const [health, auth, providers] = await Promise.all([
            this._client.health(),
            this._client.authStatus().catch(() => undefined),
            this._client.listProviders().catch(() => undefined),
          ]);
          const provider = providers?.providers.find((p) => p.name === providers.default_provider || p.is_default);
          const authState = classifyAuthDisplayState(auth);
          const authLabel = authState === 'expired'
            ? vscode.l10n.t('expired')
            : authState === 'signed_in'
              ? vscode.l10n.t('signed in')
              : vscode.l10n.t('not signed in');
          this._postSlashInfo([
            `Daemon: ${health.service} ${health.version}`,
            `Auth: ${authLabel}`,
            `Provider: ${provider ? `${provider.name} (${provider.model})` : vscode.l10n.t('not configured')}`,
          ].join('\n'), sessionId, text);
        } catch (e) {
          this._postSlashInfo(vscode.l10n.t('Unable to read status: {message}', { message: this._messageFromError(e) }), sessionId, text);
        }
        return true;
      case '/config':
        try {
          const config = await this._client.getConfig();
          const provider = config.providers.find((p) => p.name === config.default_provider || p.is_default);
          this._postSlashInfo([
            `Provider: ${provider ? `${provider.name} (${provider.model})` : config.default_provider || vscode.l10n.t('not configured')}`,
            `Config: ${config.path}`,
            '',
            vscode.l10n.t('Example:'),
            '',
            '```toml',
            'default_provider = "deepseek"',
            '',
            '[providers.deepseek]',
            'type           = "openai"',
            'api_key        = "sk-..."',
            'model          = "deepseek-chat"',
            'base_url       = "https://api.deepseek.com/v1"',
            'context_window = 64000',
            '```',
            '',
            vscode.l10n.t('Full reference: docs/config.example.toml'),
            vscode.l10n.t('Edit the file, then run /reload. No restart needed.'),
          ].join('\n'), sessionId, text);
        } catch (e) {
          this._postSlashInfo(vscode.l10n.t('Unable to read config: {message}', { message: this._messageFromError(e) }), sessionId, text);
        }
        return true;
      case '/reload':
        try {
          const config = await this._client.reloadConfig();
          const provider = config.providers.find((p) => p.name === config.default_provider || p.is_default);
          await this._sendSetupState();
          this._postSlashInfo(
            vscode.l10n.t('Reloaded config. Default provider: {provider}.', { provider: provider?.name || config.default_provider || 'none' }),
            sessionId,
            text,
          );
        } catch (e) {
          this._postSlashInfo(vscode.l10n.t('Unable to reload config: {message}', { message: this._messageFromError(e) }), sessionId, text);
        }
        return true;
      default:
        return false;
    }
  }

  private _postSlashInfo(text: string, sessionId?: string, userText?: string) {
    if (sessionId) this._postMessageForSession(sessionId, { type: 'assistantMessage', text });
    else this._postMessage({ type: 'assistantMessage', text });
    if (sessionId && userText) {
      void this._persistLocalCommandTranscript(sessionId, userText, text);
    }
  }

  private async _persistLocalCommandTranscript(sessionId: string, userText: string, assistantText: string) {
    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    try {
      const result = await this._client.appendSessionMessages(sessionId, {
        working_dir: workspaceFolder,
        messages: [
          { role: 'user', content: userText },
          { role: 'assistant', content: assistantText },
        ],
      });
      const info = this._panelSessions.get(sessionId);
      if (info) {
        info.projectHash = result.project_hash;
        // Ensure cached messages are loaded before appending
        if (!info.messages && info.messagesPromise) {
          info.messages = (await info.messagesPromise) ?? [];
          info.messagesPromise = undefined;
        }
        if (info.messages) {
          info.messages = [
            ...info.messages,
            { role: 'user', content: userText },
            { role: 'assistant', content: assistantText },
          ];
        }
      }
      this._getRuntime(sessionId).projectHash = result.project_hash;
      await this._refreshSessions();
    } catch (e) {
      console.warn(`[AtomCode] Failed to persist local slash command: ${this._messageFromError(e)}`);
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
        s.hasUnread = false;
      }
    }
  }

  private async _refreshSessions() {
    try {
      const loaded = await this._loadSessionsForDisplay();
      const sessions = loaded.sessions;
      await this._annotateSessionGenerating(sessions as any[]);
      // If we have panel sessions that the daemon filtered out (e.g. newly
      // created with no messages yet), prepend synthetic entries so they appear
      // in the session list immediately.
      const existingIds = new Set(sessions.map((s: any) => s.meta?.id || s.id));
      for (const [, info] of this._panelSessions) {
        const belongsToCurrentWorkspace = !loaded.workspaceFolder
          || (loaded.currentProjectHash && info.projectHash === loaded.currentProjectHash)
          || info.workingDir === loaded.workspaceFolder;
        if (!belongsToCurrentWorkspace) {
          continue;
        }
        if (!existingIds.has(info.sessionId)) {
          sessions.unshift({
            id: info.sessionId,
            name: vscode.l10n.t('New session'),
            created_at: Date.now(),
            updated_at: Date.now(),
            project_hash: info.projectHash,
            isGenerating: this._sessionRuntimes.get(info.sessionId)?.isGenerating ?? false,
            hasUnread: false,
          } as any);
          existingIds.add(info.sessionId);
        }
      }
      this._broadcastMessage({ type: 'sessions', sessions });

      // Update panel tab titles to reflect session names
      const nameById = new Map<string, string>();
      for (const s of sessions as any[]) {
        const sid = s.meta?.id || s.id;
        const label = s.name || s.title;
        if (sid && label) nameById.set(sid, label);
      }
      for (const [sid, panel] of this._panels) {
        const label = nameById.get(sid);
        if (label && panel.title !== label) {
          panel.title = label;
        }
      }
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

  private _postOrQueueToPanel(sessionId: string, msg: any, generation?: number) {
    if (this._panelReady.get(sessionId)) {
      this._postMessageToPanel(sessionId, msg);
    } else if (this._panels.has(sessionId)) {
      // Generation-bound history/terminal messages must never cross into a
      // replacement turn while a webview is rebuilding.
      const queue = this._pendingMessages.get(sessionId) || [];
      queue.push({ message: msg, generation });
      this._pendingMessages.set(sessionId, queue);
    }
    if (this._activeSessionId === sessionId) {
      this._view?.webview.postMessage(msg);
    }
  }

  private _flushPendingMessages(
    sessionId: string,
    terminalAlreadyDeliveredGeneration?: number,
    historyAlreadyDeliveredGeneration?: number,
  ): boolean {
    const queue = this._pendingMessages.get(sessionId);
    if (!queue || queue.length === 0) return false;
    this._pendingMessages.delete(sessionId);
    const generation = this._sessionRuntimes.get(sessionId)?.streamGeneration ?? 0;
    let terminalIncluded = false;
    for (const entry of queue) {
      if (entry.generation !== undefined && entry.generation !== generation) continue;
      if (
        entry.message?.type === 'sessionMessages'
        && historyAlreadyDeliveredGeneration !== undefined
        && entry.generation === historyAlreadyDeliveredGeneration
      ) continue;
      const isTerminal = entry.message?.type === 'done'
        || entry.message?.type === 'stopped'
        || entry.message?.type === 'error'
        || entry.message?.type === 'generationStopped';
      if (
        isTerminal
        && terminalAlreadyDeliveredGeneration !== undefined
        && entry.generation === terminalAlreadyDeliveredGeneration
      ) continue;
      this._postMessageToPanel(sessionId, entry.message);
      terminalIncluded ||= entry.message?.type === 'sessionMessages' && Boolean(entry.message.terminal);
    }
    return terminalIncluded;
  }

  private _postMessage(msg: any, webview?: vscode.Webview) {
    if (webview) {
      webview.postMessage(msg);
      return;
    }
    // Route to focused panel, fallback to first panel, then sidebar
    if (this._focusedPanelId) {
      const panel = this._panels.get(this._focusedPanelId);
      if (panel) { panel.webview.postMessage(msg); return; }
    }
    // Fallback to any open panel
    const firstPanel = this._panels.values().next().value;
    if (firstPanel) { firstPanel.webview.postMessage(msg); return; }
    // Last resort: sidebar
    this._view?.webview.postMessage(msg);
  }

  private _postMessageToPanel(sessionId: string, msg: any) {
    const panel = this._panels.get(sessionId);
    if (panel) {
      panel.webview.postMessage(msg);
    }
  }

  // While a tab is rebuilding its initial state, the event buffer is the sole stream
  // source. Forwarding the same live event before replay finishes would render it twice.
  private _postStreamEventIfReady(sessionId: string, msg: any) {
    if (this._panelReady.get(sessionId)) {
      this._postMessageToPanel(sessionId, msg);
    }
    if (this._activeSessionId === sessionId) {
      this._view?.webview.postMessage(msg);
    }
  }

  private _postTerminalForSession(sessionId: string, msg: any, generation?: number) {
    if (this._panels.has(sessionId) && !this._panelReady.get(sessionId)) {
      const queue = this._pendingMessages.get(sessionId) || [];
      queue.push({ message: msg, generation });
      this._pendingMessages.set(sessionId, queue);
    } else {
      this._postMessageToPanel(sessionId, msg);
    }
    if (this._activeSessionId === sessionId) {
      this._view?.webview.postMessage(msg);
    }
  }

  // Close the ready-handshake race without double-rendering. `_sendInitialState`
  // records the generation and buffer length it replayed while the panel stayed
  // not-ready. Any events appended before that async initialization returned are
  // replayed from the cursor; if a new generation replaced the buffer, replay it
  // from the beginning. Pending history and terminals are filtered by that same
  // generation before the panel becomes live.
  private _finishPanelReadyReplay(webview: vscode.Webview, cursor: StreamReplayCursor) {
    const sid = cursor.sessionId;
    let currentTerminal: SessionTerminal | undefined;
    if (sid) {
      const rt = this._sessionRuntimes.get(sid);
      if (rt?.isGenerating) {
        const fromIndex = (rt.streamGeneration ?? 0) === cursor.streamGeneration
          ? cursor.replayedEvents
          : 0;
        this._replayStreamBuffer(sid, rt, webview, fromIndex);
      }
      const generation = rt?.streamGeneration ?? 0;
      currentTerminal = rt?.terminal?.generation === generation ? rt.terminal : undefined;
    }
    this._markPanelReady(webview);
    if (sid) {
      const historyIncludedTerminal = this._flushPendingMessages(
        sid,
        cursor.terminalGeneration,
        cursor.historyGeneration,
      );
      if (
        currentTerminal
        && cursor.terminalGeneration !== currentTerminal.generation
        && !historyIncludedTerminal
      ) {
        this._postMessageToPanel(sid, this._terminalForWebview(currentTerminal));
      }
    }
  }

  private _postMessageForSession(sessionId: string, msg: any) {
    this._postOrQueueToPanel(sessionId, msg);
  }

  private _terminalForWebview(terminal?: SessionTerminal): any | undefined {
    if (!terminal) return undefined;
    const { generation: _generation, ...message } = terminal;
    return message;
  }

  private _broadcastToPanels(msg: any) {
    for (const webview of this._webviewPanels.keys()) {
      webview.postMessage(msg);
    }
  }

  private _broadcastMessage(msg: any) {
    this._view?.webview.postMessage(msg);
    this._broadcastToPanels(msg);
  }

  private _markPanelReady(webview: vscode.Webview) {
    const sid = this._sessionIdForWebview(webview);
    if (sid) this._panelReady.set(sid, true);
  }

  private _markPanelNotReady(webview: vscode.Webview) {
    const sid = this._sessionIdForWebview(webview);
    if (sid) this._panelReady.set(sid, false);
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
    html = html.replace(/\{\{locale\}\}/g, vscode.env.language || 'en');
    // VS Code injects `editor.fontFamily` as `--vscode-editor-font-family` but NOT
    // `chatEditor.fontFamily` (that setting is for the built-in chat only). Resolve the chat
    // font ourselves and, when set, override the monospace var so code blocks AND the input
    // honor it. Empty → no override, so the CSS default (`--vscode-editor-font-family`) wins.
    const font = resolveChatFontFamily();
    // Always emit the element (empty when no font) with a stable id, so the live-update path
    // (`chromeFont`) can edit THIS SAME rule — setting/clearing its text — instead of an inline
    // `documentElement.style`, which could not clear a value that lives in a stylesheet rule.
    html = html.replace(
      /\{\{fontStyle\}\}/g,
      `<style id="atomcode-chat-font">${font ? `:root{--app-monospace-font-family:${font};}` : ''}</style>`,
    );

    return html;
  }
}

/**
 * Read the chat monospace font from config and sanitize it for safe inlining into a
 * `<style>` block. Precedence: `atomcode.chat.fontFamily` (our own setting) > VS Code's
 * `chatEditor.fontFamily`. Returns `undefined` when neither is set (fall back to the CSS
 * default, i.e. `editor.fontFamily`). The sanitizer strips anything that could break out of
 * the CSS value (`<`, `>`, `{`, `}`, `;`, `:`, backslash, …) — a font-family value only ever
 * needs letters, digits, spaces, quotes, commas, dots and hyphens.
 */
function resolveChatFontFamily(): string | undefined {
  // Scoped reads, matching the rest of this file (`getConfiguration('atomcode')`): our own
  // key, else VS Code's built-in `chatEditor.fontFamily`. `editor.fontFamily` is deliberately
  // NOT read here — it already reaches the webview as `--vscode-editor-font-family` (the CSS
  // default), so an empty result correctly falls through to it with no override.
  const raw =
    vscode.workspace.getConfiguration('atomcode').get<string>('chat.fontFamily', '').trim() ||
    vscode.workspace.getConfiguration('chatEditor').get<string>('fontFamily', '').trim();
  if (!raw) {
    return undefined;
  }
  // A font-family value only needs letters, digits, spaces, quotes, commas, dots and hyphens.
  // Strip everything else — incl. `<>{};:()\` and `url` would be inert but we drop `()` too —
  // so the value cannot break out of the inlined `--app-monospace-font-family:VALUE;` declaration.
  const safe = raw.replace(/[^a-zA-Z0-9 ,._'"-]/g, '').slice(0, 200).trim();
  return safe || undefined;
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
