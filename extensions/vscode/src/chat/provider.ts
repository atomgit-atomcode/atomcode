import * as vscode from 'vscode';
import * as path from 'path';
import * as fs from 'fs';
import { DaemonClient } from '../daemon/client';
import { ChatRequest } from '../daemon/types';

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _currentAbort?: AbortController;
  private _sessionId?: string;
  private _isGenerating = false;

  public onModelSelected?: (model: string) => void;

  constructor(
    private readonly _extensionUri: vscode.Uri,
    private readonly _client: DaemonClient,
  ) {}

  resolveWebviewView(webviewView: vscode.WebviewView) {
    this._view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this._extensionUri, 'webview'),
        vscode.Uri.joinPath(this._extensionUri, 'node_modules', 'highlight.js'),
      ],
    };
    webviewView.webview.html = this._getHtml(webviewView.webview);

    // Handle messages from webview
    webviewView.webview.onDidReceiveMessage(async (msg) => {
      switch (msg.type) {
        case 'send':
          await this._handleSend(msg.text);
          break;
        case 'stop':
          this.stopGeneration();
          break;
        case 'newConversation':
          this.newConversation();
          break;
        case 'ready':
          // Webview loaded, send initial state
          await this._sendInitialState();
          break;
        case 'selectModel':
          this.onModelSelected?.(msg.model);
          break;
        case 'loadSession':
          await this._loadSession(msg.sessionId);
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
      }
    });

    // Set context for keybindings
    webviewView.onDidChangeVisibility(() => {
      vscode.commands.executeCommand('setContext', 'atomcode.chatFocused', webviewView.visible);
    });
  }

  // Public API for commands
  public async sendMessage(text: string) {
    this._postMessage({ type: 'userMessage', text });
    await this._handleSend(text);
  }

  public newConversation() {
    this._sessionId = undefined;
    this._postMessage({ type: 'clearChat' });
  }

  public stopGeneration() {
    this._currentAbort?.abort();
    this._currentAbort = undefined;
    this._isGenerating = false;
    this._postMessage({ type: 'generationStopped' });
  }

  public focusInput() {
    this._view?.show(true);
    this._postMessage({ type: 'focusInput' });
  }

  // Private
  private async _handleSend(text: string) {
    if (!text.trim() || this._isGenerating) return;

    this._isGenerating = true;
    this._postMessage({ type: 'generationStarted' });

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const request: ChatRequest = {
      message: text,
      working_dir: workspaceFolder,
      session_id: this._sessionId,
    };

    this._currentAbort = this._client.streamChat(request, {
      onText: (content) => this._postMessage({ type: 'text', content }),
      onToolStart: (name, args) => this._postMessage({ type: 'toolStart', name, args }),
      onToolResult: (name, output, success, durationMs) =>
        this._postMessage({ type: 'toolResult', name, output, success, durationMs }),
      onTokens: (prompt, completion, total) =>
        this._postMessage({ type: 'tokens', prompt, completion, total }),
      onArtifactStart: (id, artifactType, language, title) =>
        this._postMessage({ type: 'artifactStart', id, artifactType, language, title }),
      onArtifactContent: (id, content) =>
        this._postMessage({ type: 'artifactContent', id, content }),
      onArtifactEnd: (id) =>
        this._postMessage({ type: 'artifactEnd', id }),
      onDone: (tokens, toolCalls) => {
        this._isGenerating = false;
        this._postMessage({ type: 'done', tokens, toolCalls });
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

  private async _sendInitialState() {
    // Send models
    try {
      const models = await this._client.listModels();
      this._postMessage({ type: 'models', models });
    } catch {}

    // Send sessions
    try {
      const sessions = await this._client.listSessions();
      this._postMessage({ type: 'sessions', sessions });
    } catch {}

    // Send editor context
    this._sendEditorContext();

    this._postMessage({ type: 'init', generating: this._isGenerating });
  }

  private _sendEditorContext() {
    const editor = vscode.window.activeTextEditor;
    if (editor) {
      const selection = editor.selection;
      this._postMessage({
        type: 'context',
        filePath: editor.document.uri.fsPath,
        fileName: path.basename(editor.document.uri.fsPath),
        selection: !selection.isEmpty ? editor.document.getText(selection) : undefined,
        language: editor.document.languageId,
      });
    }
  }

  private async _loadSession(sessionId: string) {
    // TODO: need to find projectHash for the session
    // For now, just start a new conversation with the session ID
    this._sessionId = sessionId;
    this._postMessage({ type: 'clearChat' });
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

  private _postMessage(msg: unknown) {
    this._view?.webview.postMessage(msg);
  }

  private _getHtml(webview: vscode.Webview): string {
    const htmlPath = vscode.Uri.joinPath(this._extensionUri, 'webview', 'index.html');
    let html = fs.readFileSync(htmlPath.fsPath, 'utf-8');

    const styleUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this._extensionUri, 'webview', 'styles.css'),
    );
    const scriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this._extensionUri, 'webview', 'main.js'),
    );
    const nonce = getNonce();

    html = html.replace(/\{\{styleUri\}\}/g, styleUri.toString());
    html = html.replace(/\{\{scriptUri\}\}/g, scriptUri.toString());
    html = html.replace(/\{\{nonce\}\}/g, nonce);
    html = html.replace(/\{\{cspSource\}\}/g, webview.cspSource);

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
