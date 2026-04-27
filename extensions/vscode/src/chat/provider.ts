import * as vscode from 'vscode';
import * as fs from 'fs';
import { DaemonClient } from '../daemon/client';
import { ChatRequest } from '../daemon/types';

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'atomcode.chatView';
  private _view?: vscode.WebviewView;
  private _currentAbort?: AbortController;
  private _sessionId?: string;
  private _isGenerating = false;

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
          this._postMessage({ type: 'init', generating: this._isGenerating });
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
