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
type ContextItem = { path: string; type: string; fileName?: string; language?: string; selection?: string; startLine?: number; endLine?: number };
type QueuedChatMessage = { text: string; context?: ContextItem[]; clientMessageId?: string };

interface SessionRuntime {
  abortController?: AbortController;
  isGenerating: boolean;
  queuedMessages: QueuedChatMessage[];
  projectHash?: string;
  errorMessage?: string;
  eventBuffer: Array<{
    type: 'userMessage' | 'text' | 'toolBatchStart' | 'toolStart' | 'toolResult' | 'tokens';
    data: any;
  }>;
}

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _panels = new Map<string, vscode.WebviewPanel>();
  private _panelSessions = new Map<string, { sessionId: string; projectHash?: string; messages?: MessageInfo[] }>();
  private _panelReady = new Map<string, boolean>();
  private _activeSessionId?: string;
  private _focusedPanelId?: string;
  private _groupLocked = false;
  private _sessionRuntimes = new Map<string, SessionRuntime>();
  private _pendingMessages = new Map<string, any[]>();
  private _loginId?: string;
  private _loginPoll?: ReturnType<typeof setInterval>;
  private _loginStartedFromCommand = false;
  public onModelSelected?: (model: string) => void;

  constructor(
    private readonly _extensionUri: vscode.Uri,
    private readonly _client: DaemonClient,
  ) {}

  public dispose() {
    this._clearLoginPoll();
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

  private _lockGroupOnce() {
    if (this._groupLocked) return;
    this._groupLocked = true;
    vscode.commands.executeCommand('workbench.action.lockEditorGroup');
  }

  public openInTab(sessionId?: string) {
    // If session is already open in a panel, reveal it
    if (sessionId) {
      const existing = this._panels.get(sessionId);
      if (existing) {
        existing.reveal();
        this._focusedPanelId = sessionId;
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
    panel.webview.html = this._getHtml(panel.webview, 'tab');
    this._setupWebviewMessageHandler(panel.webview, 'tab');

    // Lock the editor group on first panel creation
    this._lockGroupOnce();

    // Track the panel — required for message routing, size check, and lookup
    if (sessionId) {
      this._panels.set(sessionId, panel);
      this._focusedPanelId = sessionId;
    }

    // When user switches to this tab in VS Code, sync sidebar selection
    panel.onDidChangeViewState((e) => {
      if (e.webviewPanel.active) {
        const activeSid = this._findSessionIdByPanel(panel);
        if (activeSid) {
          this._focusedPanelId = activeSid;
          const info = this._panelSessions.get(activeSid);
          this._broadcastMessage({ type: 'sessionSelected', sessionId: activeSid, projectHash: info?.projectHash });
        }
      }
    });

    panel.onDidDispose(() => {
      const disposedSid = this._findSessionIdByPanel(panel);
      if (disposedSid) {
        this._panels.delete(disposedSid);
        this._panelReady.delete(disposedSid);
        this._panelSessions.delete(disposedSid);
        if (this._focusedPanelId === disposedSid) {
          this._focusedPanelId = undefined;
        }
        // Only clean up runtime if no other panel uses this session
        const stillInUse = Array.from(this._panelSessions.values())
          .some(s => s.sessionId === disposedSid);
        if (!stillInUse) {
          const rt = this._sessionRuntimes.get(disposedSid);
          if (rt?.isGenerating) {
            rt.abortController?.abort();
            void this._client.stopGeneration(disposedSid).catch(() => undefined);
          }
        }
      }
    });
  }

  private _findSessionIdByPanel(panel: vscode.WebviewPanel): string | undefined {
    for (const [sid, p] of this._panels) {
      if (p === panel) return sid;
    }
    return undefined;
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

  public setupPanelForRestore(panel: vscode.WebviewPanel, sessionId?: string, projectHash?: string) {
    panel.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this._extensionUri, 'webview'),
        vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
      ],
    };
    panel.webview.html = this._getHtml(panel.webview, 'tab');
    panel.iconPath = {
      light: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
      dark: vscode.Uri.joinPath(this._extensionUri, 'resources', 'icon.svg'),
    };
    this._setupWebviewMessageHandler(panel.webview, 'tab');

    // Track the panel
    if (sessionId) {
      this._panels.set(sessionId, panel);
    }

    if (sessionId && projectHash) {
      this._panelSessions.set(sessionId, { sessionId, projectHash });
      // Async: load messages from daemon when ready
      this._client.getSession(projectHash, sessionId).then(detail => {
        const info = this._panelSessions.get(sessionId);
        if (info && detail?.messages) {
          info.messages = detail.messages;
        }
      }).catch(() => { /* session may not exist yet */ });
    }

    // When user switches to this tab in VS Code, sync sidebar selection
    panel.onDidChangeViewState((e) => {
      if (e.webviewPanel.active) {
        const activeSid = this._findSessionIdByPanel(panel);
        if (activeSid) {
          this._focusedPanelId = activeSid;
          const info = this._panelSessions.get(activeSid);
          this._broadcastMessage({ type: 'sessionSelected', sessionId: activeSid, projectHash: info?.projectHash });
        }
      }
    });

    panel.onDidDispose(() => {
      const disposedSid = this._findSessionIdByPanel(panel);
      if (disposedSid) {
        this._panels.delete(disposedSid);
        this._panelReady.delete(disposedSid);
        this._panelSessions.delete(disposedSid);
        if (this._focusedPanelId === disposedSid) {
          this._focusedPanelId = undefined;
        }
      }
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
    });
  }

  private _setupWebviewMessageHandler(webview: vscode.Webview, mode: WebviewMode) {
    webview.onDidReceiveMessage(async (msg) => {
      switch (msg.type) {
        case 'send':
          await this._handleSend(
            msg.text,
            msg.context,
            msg.clientMessageId,
            msg.sessionId,
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
          // Flush any messages queued before the webview was ready
          {
            let sid: string | undefined;
            for (const [s, panel] of this._panels) {
              if (panel.webview === webview) { sid = s; break; }
            }
            if (sid) this._flushPendingMessages(sid);
          }
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
            if (msg.startLine) {
              const start = msg.startLine - 1;
              const end = msg.endLine ? msg.endLine - 1 : start;
              opts.selection = new vscode.Range(start, 0, end, 0);
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
      }
    });
  }

  // Public API for commands
  public async sendMessage(text: string) {
    const sid = this._focusedPanelId;
    if (!sid) {
      await this.newConversation();
      const newSid = this._focusedPanelId;
      if (!newSid) return;
      this._postMessageToPanel(newSid, { type: 'userMessage', text });
      return;
    }
    this._postMessageToPanel(sid, { type: 'userMessage', text });
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
      this._panelSessions.set(sid, { sessionId: sid, projectHash: session.project_hash });
      this.openInTab(sid);
      await this._refreshSessions();
    } else {
      const rt = this._sessionRuntimes.get(sid);
      if (rt?.isGenerating) {
        this.stopGeneration();
      }
      await this._ensureSession(sid);
      this._postMessageToPanel(sid, { type: 'clearChat' });
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
      this._panelSessions.set(sid, { sessionId: sid, projectHash: session.project_hash });
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
      this._broadcastMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
      return;
    }
    sessionId = session.id;
    projectHash = session.project_hash;
    this._getRuntime(sessionId).projectHash = session.project_hash;

    // Open new tab for the new session
    this._panelSessions.set(sessionId, { sessionId, projectHash });
    this.openInTab(sessionId);

    // Notify all panels + sidebar about the new active session
    this._broadcastMessage({ type: 'sessionSelected', sessionId, projectHash });
    await this._refreshSessions();
  }

  public stopGeneration() {
    const sid = this._focusedPanelId;
    if (!sid) return;
    const rt = this._sessionRuntimes.get(sid);
    if (!rt?.isGenerating) return;

    rt.abortController?.abort();
    rt.abortController = undefined;
    rt.queuedMessages = [];
    rt.isGenerating = false;
    rt.eventBuffer = [];
    void this._client.stopGeneration(sid).catch(() => undefined);
    this._postMessageToPanel(sid, { type: 'generationStopped' });
  }

  public focusInput() {
    const sid = this._focusedPanelId;
    if (sid) {
      this._postMessageToPanel(sid, { type: 'focusInput' });
    }
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

  private async _handleSend(text: string, context?: Array<{ path: string; type: string; fileName?: string; language?: string; selection?: string; startLine?: number; endLine?: number }>, clientMessageId?: string, msgSessionId?: string) {
    const trimmed = text.trim();
    if (!trimmed) return;

    let sid = msgSessionId ?? this._focusedPanelId;
    if (!sid) {
      sid = await this._ensureSession();
    }
    if (!sid) return;
    const rt = this._getRuntime(sid);

    if (rt.isGenerating) {
      rt.queuedMessages.push({ text: trimmed, context, clientMessageId });
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
    };

    // Capture session ID so callbacks always reference the correct session
    const streamSessionId = sid;

    rt.abortController = this._client.streamChat(request, {
      onText: (content) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'text', data: { content } });
        this._postMessageToPanel(streamSessionId, { type: 'text', content });
      },
      onToolBatch: (calls) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolBatchStart' as const, data: { calls } });
        this._postMessageToPanel(streamSessionId, { type: 'toolBatchStart', calls });
      },
      onToolStart: (id, name, args) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolStart', data: { id, name, args } });
        this._postMessageToPanel(streamSessionId, { type: 'toolStart', id, name, args });
      },
      onToolResult: (id, name, output, success, durationMs) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'toolResult', data: { id, name, output, success, durationMs } });
        this._postMessageToPanel(streamSessionId, { type: 'toolResult', id, name, output, success, durationMs });
      },
      onTokens: (prompt, completion, total) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.eventBuffer.push({ type: 'tokens', data: { prompt, completion, total } });
        this._postMessageToPanel(streamSessionId, { type: 'tokens', prompt, completion, total });
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

        if (sessionId && sessionId !== streamSessionId) {
          this._sessionRuntimes.set(sessionId, srt);
          this._sessionRuntimes.delete(streamSessionId);
          // Update panel bindings
          this._panels.set(sessionId, this._panels.get(streamSessionId)!);
          this._panels.delete(streamSessionId);
          const info = this._panelSessions.get(streamSessionId);
          if (info) {
            info.sessionId = sessionId;
            this._panelSessions.set(sessionId, info);
            this._panelSessions.delete(streamSessionId);
          }
          if (this._focusedPanelId === streamSessionId) {
            this._focusedPanelId = sessionId;
          }
        }

        this._postMessageToPanel(sessionId || streamSessionId, { type: 'done', tokens, toolCalls, sessionId });
        void this._refreshSessions();
        setTimeout(() => void this._sendNextQueuedMessage(), 75);
      },
      onStopped: () => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.isGenerating = false;
        srt.queuedMessages = [];
        srt.eventBuffer = [];
        this._postMessageToPanel(streamSessionId, { type: 'stopped' });
      },
      onError: (message) => {
        const srt = this._sessionRuntimes.get(streamSessionId);
        if (!srt) return;
        srt.isGenerating = false;
        srt.queuedMessages = [];
        srt.eventBuffer = [];
        this._postMessageToPanel(streamSessionId, { type: 'error', message });
      },
    });
  }

  private async _sendNextQueuedMessage() {
    const sid = this._focusedPanelId;
    if (!sid) return;
    const rt = this._sessionRuntimes.get(sid);
    if (!rt || rt.isGenerating) return;
    const next = rt.queuedMessages.shift();
    if (!next) return;
    await this._handleSend(next.text, next.context, next.clientMessageId);
    const rt2 = this._sessionRuntimes.get(sid);
    if (rt2 && !rt2.isGenerating) {
      void this._sendNextQueuedMessage();
    }
  }

  private async _ensureSession(forPanelSessionId?: string): Promise<string | undefined> {
    const sid = forPanelSessionId ?? this._focusedPanelId;
    if (sid) {
      const rt = this._sessionRuntimes.get(sid);
      if (rt) return sid;
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const session = await this._client.createSession(undefined, workspaceFolder);
    this._getRuntime(session.id).projectHash = session.project_hash;

    if (sid) {
      this._panelSessions.set(sid, { sessionId: session.id, projectHash: session.project_hash });
    }

    await this._refreshSessions();
    return session.id;
  }


  public sendEditorContext() {
    this._sendEditorContext();
  }

  // New protocol methods

  private async _sendInitialState(webview?: vscode.Webview, mode: WebviewMode = 'tab') {
    let currentModelName = '';
    let sid: string | undefined;
    let messagesToLoad: MessageInfo[] | undefined;

    // Find session binding for this panel
    if (webview && mode === 'tab') {
      for (const [s, info] of this._panelSessions) {
        const panel = this._panels.get(s);
        if (panel?.webview === webview) {
          sid = info.sessionId;
          messagesToLoad = info.messages;
          info.messages = undefined;
          break;
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

    try {
      const sessions = await this._client.listSessions();
      await this._annotateSessionGenerating(sessions as any[]);
      this._postMessage({ type: 'sessions', sessions }, webview);
    } catch {}

    this._sendEditorContext(webview);

    if (messagesToLoad && mode === 'tab') {
      this._postMessage({ type: 'sessionMessages', messages: messagesToLoad }, webview);
    }

    if (sid) {
      const rt = this._sessionRuntimes.get(sid);
      if (rt) {
        this._replayStreamBuffer(sid, rt, webview);
        if (rt.errorMessage) {
          this._postMessage({ type: 'error', message: rt.errorMessage }, webview);
          rt.errorMessage = undefined;
        }
      }
    }

    const projectHash = sid ? (this._panelSessions.get(sid)?.projectHash ?? this._sessionRuntimes.get(sid)?.projectHash) : undefined;

    this._postMessage({
      type: 'init',
      generating: sid ? (this._sessionRuntimes.get(sid)?.isGenerating ?? false) : false,
      currentModel: currentModelName,
      viewMode: mode,
      activeSessionId: sid,
      projectHash,
      isSessionList: mode === 'sidebar',
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
      this._broadcastMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });

      this._loginPoll = setInterval(() => {
        void this._pollLogin();
      }, 2000);
      await this._pollLogin();
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
    }
  }

  private async _pollLogin() {
    if (!this._loginId) return;
    try {
      const result = await this._client.pollLogin(this._loginId);
      if (result.status === 'pending') {
        this._broadcastMessage({ type: 'loginPending' });
        return;
      }
      this._clearLoginPoll();
      this._loginId = undefined;
      this._broadcastMessage({ type: 'loginAuthorized', user: result.user });
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
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
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
      this._broadcastMessage({ type: 'setupWorking', message: 'Waiting for AtomGit sign-in...' });

      await this._cancelLogin();
      const login = await this._client.startLogin(true);
      this._loginId = login.login_id;
      this._broadcastMessage({ type: 'loginStarted', loginId: login.login_id, url: login.url });

      while (this._loginId === login.login_id) {
        const result = await this._client.pollLogin(login.login_id);
        if (result.status === 'pending') {
          this._broadcastMessage({ type: 'loginPending' });
          await delay(2000);
          continue;
        }

        this._loginId = undefined;
        this._broadcastMessage({ type: 'loginAuthorized', user: result.user });
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
      this._broadcastMessage({ type: 'setupError', message });
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
      this._broadcastMessage({ type: 'setupWorking', message: 'Syncing CodingPlan models...' });
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
    }
  }

  private async _deleteProvider(name: string) {
    try {
      await this._client.deleteProvider(name);
      await this._sendSetupState();
    } catch (e) {
      this._broadcastMessage({ type: 'setupError', message: this._messageFromError(e) });
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
      existing.reveal();
      this._focusedPanelId = sessionId;
      return;
    }

    let hash = projectHash;
    if (!hash) {
      try {
        const allSessions = await this._client.listSessions();
        const match = (allSessions as Array<{ project_hash?: string; meta?: { id?: string }; id?: string }>)
          .find(s => (s.meta?.id || s.id) === sessionId);
        hash = match?.project_hash ?? this._sessionRuntimes.get(sessionId)?.projectHash;
      } catch { /* proceed without hash */ }
    }

    if (!hash) {
      vscode.window.showErrorMessage('Unable to open session: missing project hash.');
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
    this._getRuntime(sessionId).projectHash = hash;
    this.openInTab(sessionId);
    this._broadcastMessage({ type: 'sessionSelected', sessionId, projectHash: hash });
    await this._refreshSessions();
  }

  // Replay buffered stream events so the webview can resume displaying a
  // background stream. Snapshot the event buffer to avoid conflicts with new
  // events arriving during replay (they will be forwarded by callbacks once
  // the panel's webview is bound).
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
      await this._deleteSessionInternal(sessionId, hash);
    } catch (e) {
      this._postMessage({ type: 'error', message: `Unable to delete session: ${this._messageFromError(e)}` });
    }
  }

  private async _deleteSessionInternal(sessionId: string, hash: string) {
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
    let cleared = false;
    for (const [pid, info] of [...this._panelSessions]) {
      if (info.sessionId === sessionId) {
        this._panelSessions.delete(pid);
        if (this._focusedPanelId === pid) {
          this._focusedPanelId = undefined;
        }
        cleared = true;
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
      cleared = true;
      panel.dispose();
    }
    if (cleared) {
      this._broadcastMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
      this._broadcastMessage({ type: 'clearChat' });
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
      ? `Delete AtomCode session "${sessions[0].name || sessions[0].sessionId}"?`
      : `确定删除 ${count} 个会话？`;

    const choice = await vscode.window.showWarningMessage(
      label,
      { modal: true, detail: '此操作不可撤销，会话将从本地历史中移除。' },
      'Delete',
    );
    if (choice !== 'Delete') return;

    let succeeded = 0;
    let failed = 0;
    for (const { sessionId, projectHash, name } of sessions) {
      const hash = await this._resolveSessionProjectHash(sessionId, projectHash);
      if (!hash) {
        // No daemon record — clean up panel bindings locally
        let cleared = false;
        for (const [pid, info] of [...this._panelSessions]) {
          if (info.sessionId === sessionId) {
            this._panelSessions.delete(pid);
            if (this._focusedPanelId === pid) {
              this._focusedPanelId = undefined;
            }
            cleared = true;
          }
        }
        const panel = this._panels.get(sessionId);
        if (panel) {
          this._panels.delete(sessionId);
          this._panelReady.delete(sessionId);
          if (this._focusedPanelId === sessionId) {
            this._focusedPanelId = undefined;
          }
          cleared = true;
          panel.dispose();
        }
        if (cleared) {
          this._broadcastMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
          this._broadcastMessage({ type: 'clearChat' });
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
      const errMsg = { type: 'error', message: `已删除 ${succeeded}/${count} 个会话，${failed} 个失败` };
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
        s.hasUnread = false;
      }
    }
  }

  private async _refreshSessions() {
    try {
      const sessions = await this._client.listSessions();
      await this._annotateSessionGenerating(sessions as any[]);
      // If we have panel sessions that the daemon filtered out (e.g. newly
      // created with no messages yet), prepend synthetic entries so they appear
      // in the session list immediately.
      const existingIds = new Set(sessions.map((s: any) => s.meta?.id || s.id));
      for (const [pid, info] of this._panelSessions) {
        if (!existingIds.has(info.sessionId)) {
          sessions.unshift({
            id: info.sessionId,
            name: 'New session',
            created_at: Date.now(),
            updated_at: Date.now(),
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

  private _postOrQueueToPanel(sessionId: string, msg: any) {
    if (this._panelReady.get(sessionId)) {
      this._postMessageToPanel(sessionId, msg);
      return;
    }
    // Panel not ready yet — queue the message until webview sends 'ready'
    const queue = this._pendingMessages.get(sessionId) || [];
    queue.push(msg);
    this._pendingMessages.set(sessionId, queue);
  }

  private _flushPendingMessages(sessionId: string) {
    const queue = this._pendingMessages.get(sessionId);
    if (!queue || queue.length === 0) return;
    this._pendingMessages.delete(sessionId);
    for (const msg of queue) {
      this._postMessageToPanel(sessionId, msg);
    }
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

  private _broadcastToPanels(msg: any) {
    for (const panel of this._panels.values()) {
      panel.webview.postMessage(msg);
    }
  }

  private _broadcastMessage(msg: any) {
    this._view?.webview.postMessage(msg);
    this._broadcastToPanels(msg);
  }

  private _markPanelReady(webview: vscode.Webview) {
    for (const [sid, panel] of this._panels) {
      if (panel.webview === webview) {
        this._panelReady.set(sid, true);
        return;
      }
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
