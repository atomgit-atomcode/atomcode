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

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _panel?: vscode.WebviewPanel;
  private _currentAbort?: AbortController;
  private _sessionId?: string;
  private _loadedMessages?: MessageInfo[];
  private _isGenerating = false;
  private _loginId?: string;
  private _loginPoll?: ReturnType<typeof setInterval>;

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
          await this._handleSend(msg.text, msg.context?.map((c: { path: string }) => c.path));
          break;
        case 'stop':
          this.stopGeneration();
          break;
        case 'newConversation':
          await this.newConversation();
          break;
        case 'ready':
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
          await this._setupCodingPlan();
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
          await this._sendSetupState();
          break;
        case 'loadSession':
          await this._loadSession(msg.sessionId, msg.projectHash);
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
    this._postMessage({ type: 'userMessage', text });
    await this._handleSend(text);
  }

  public async newConversation() {
    this.openInTab();
    this._sessionId = undefined;
    this._loadedMessages = undefined;

    try {
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const session = await this._client.createSession(undefined, workspaceFolder);
      this._sessionId = session.id;
      this._postMessage({ type: 'sessionSelected', sessionId: session.id, projectHash: session.project_hash });
      await this._refreshSessions();
    } catch {
      this._postMessage({ type: 'sessionSelected', sessionId: undefined, projectHash: undefined });
    }

    this._postMessage({ type: 'clearChat' });
    this.focusInput();
  }

  public stopGeneration() {
    this._currentAbort?.abort();
    if (this._sessionId) {
      void this._client.stopGeneration(this._sessionId).catch(() => undefined);
    }
    this._currentAbort = undefined;
    this._isGenerating = false;
    this._postMessage({ type: 'generationStopped' });
  }

  public focusInput() {
    if (!this._panel) {
      this._view?.show(true);
    }
    this._postMessage({ type: 'focusInput' });
  }

  // Private
  private async _handleSend(text: string, contextPaths?: string[]) {
    if (!text.trim() || this._isGenerating) return;

    this._isGenerating = true;
    this._postMessage({ type: 'generationStarted' });

    // Build message with file context
    let fullMessage = text;
    if (contextPaths && contextPaths.length > 0) {
      const parts: string[] = [];
      for (const filePath of contextPaths) {
        try {
          const uri = vscode.Uri.file(filePath);
          const content = await vscode.workspace.fs.readFile(uri);
          const decoded = Buffer.from(content).toString('utf-8');
          const fileName = path.basename(filePath);
          const ext = path.extname(filePath).slice(1);
          parts.push(`File: ${fileName}\n\`\`\`${ext}\n${decoded}\n\`\`\``);
        } catch {
          // Skip files that can't be read
        }
      }
      if (parts.length > 0) {
        fullMessage = 'The user has attached the following file(s) for context. The content is provided inline below — DO NOT use read_file to re-read them.\n\n'
          + parts.join('\n\n') + '\n\n' + 'User question: ' + text;
      }
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const request: ChatRequest = {
      message: fullMessage,
      working_dir: workspaceFolder,
      session_id: this._sessionId,
    };

    this._currentAbort = this._client.streamChat(request, {
      onText: (content) => this._postMessage({ type: 'text', content }),
      onToolStart: (id, name, args) => this._postMessage({ type: 'toolStart', id, name, args }),
      onToolResult: (id, name, output, success, durationMs) =>
        this._postMessage({ type: 'toolResult', id, name, output, success, durationMs }),
      onTokens: (prompt, completion, total) =>
        this._postMessage({ type: 'tokens', prompt, completion, total }),
      onArtifactStart: (id, artifactType, language, title) =>
        this._postMessage({ type: 'artifactStart', id, artifactType, language, title }),
      onArtifactContent: (id, content) =>
        this._postMessage({ type: 'artifactContent', id, content }),
      onArtifactEnd: (id) =>
        this._postMessage({ type: 'artifactEnd', id }),
      onDone: (tokens, toolCalls, sessionId) => {
        if (sessionId) {
          this._sessionId = sessionId;
          this._loadedMessages = undefined;
          this._postMessage({ type: 'sessionSelected', sessionId });
        }
        this._isGenerating = false;
        this._postMessage({ type: 'done', tokens, toolCalls, sessionId });
        void this._refreshSessions();
      },
      onStopped: () => {
        this._isGenerating = false;
        this._postMessage({ type: 'stopped' });
      },
      onError: (message) => {
        this._isGenerating = false;
        this._postMessage({ type: 'error', message });
      },
    });
  }

  public sendEditorContext() {
    this._sendEditorContext();
  }

  // New protocol methods

  private async _sendInitialState(webview?: vscode.Webview, mode: WebviewMode = 'tab') {
    let currentModelName = '';

    await this._sendSetupState();

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
      this._postMessage({ type: 'sessions', sessions }, webview);
    } catch {}

    // Send editor context
    this._sendEditorContext(webview);

    this._postMessage({
      type: 'init',
      generating: this._isGenerating,
      currentModel: currentModelName,
      viewMode: mode,
      activeSessionId: this._sessionId,
    }, webview);

    if (this._loadedMessages && mode === 'tab') {
      this._postMessage({ type: 'sessionMessages', messages: this._loadedMessages }, webview);
    }
  }

  private async _sendSetupState() {
    let auth: AuthStatusResponse | undefined;
    let providers: ProvidersResponse | undefined;
    let config: ConfigResponse | undefined;
    let models: ModelInfo[] | undefined;

    try {
      auth = await this._client.authStatus();
      this._postMessage({ type: 'authStatus', auth });
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      providers = await this._client.listProviders();
      this._postMessage({ type: 'providers', providers: providers.providers, defaultProvider: providers.default_provider });
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
    }

    try {
      config = await this._client.getConfig();
      this._postMessage({ type: 'config', config });
    } catch {
      // Older daemons may not have P0 APIs; provider fetch error already surfaces enough.
    }

    try {
      models = await this._client.listModels();
      this._postMessage({ type: 'models', models });
    } catch {}

    const defaultProvider = providers?.providers.find((p) => p.is_default);
    this._postMessage({
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
      await this._sendSetupState();
    } catch (e) {
      this._clearLoginPoll();
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
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

  private async _setupCodingPlan() {
    try {
      this._postMessage({ type: 'setupWorking', message: 'Syncing CodingPlan models...' });
      const result: CodingPlanSetupResponse = await this._client.setupCodingPlan(this._loginId);
      this._postMessage({ type: 'codingPlanResult', result });
      await this._sendSetupState();
    } catch (e) {
      this._postMessage({ type: 'setupError', message: this._messageFromError(e) });
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
    try {
      // If projectHash not provided, search sessions to find it
      let hash = projectHash;
      if (!hash) {
        const allSessions = await this._client.listSessions();
        const match = (allSessions as Array<{ project_hash?: string; meta?: { id?: string }; id?: string }>)
          .find(s => (s.meta?.id || s.id) === sessionId);
        hash = match?.project_hash;
      }
      if (!hash) {
        this._postMessage({ type: 'error', message: 'Unable to load session: missing project hash.' });
        return;
      }

      const detail = await this._client.getSession(hash, sessionId);
      if (detail && detail.messages) {
        this._sessionId = sessionId;
        this._loadedMessages = detail.messages;
        this.openInTab();
        this._postMessage({ type: 'sessionSelected', sessionId, projectHash: hash });
        this._postMessage({ type: 'clearChat' });
        this._postMessage({ type: 'sessionMessages', messages: detail.messages });
        this.focusInput();
      } else {
        this._postMessage({ type: 'error', message: 'Unable to load session: empty response.' });
      }
    } catch (e) {
      this._postMessage({ type: 'error', message: `Unable to load session: ${this._messageFromError(e)}` });
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

  private async _searchSessions(query: string) {
    try {
      const sessions = await this._client.searchSessions(query);
      this._postMessage({ type: 'sessions', sessions });
    } catch {}
  }

  private async _refreshSessions() {
    try {
      const sessions = await this._client.listSessions();
      this._postMessage({ type: 'sessions', sessions });
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
    this._view?.webview.postMessage(msg);
    this._panel?.webview.postMessage(msg);
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
