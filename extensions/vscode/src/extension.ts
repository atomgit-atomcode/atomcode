import * as vscode from 'vscode';
import { DaemonClient } from './daemon/client';
import { DaemonProcess } from './daemon/process';
import { ChatViewProvider } from './chat/provider';
import { StatusBarManager } from './status';
import { AtomCodeActionProvider } from './editor/actions';
import { DiffContentProvider } from './editor/diff';
import { getEditorContext, buildContextualPrompt } from './editor/context';

let client: DaemonClient;
let daemonProcess: DaemonProcess;
let chatProvider: ChatViewProvider;
let statusBar: StatusBarManager;
let healthCheckInterval: ReturnType<typeof setInterval>;

export async function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration('atomcode');
  const port = config.get<number>('daemon.port', 23462);

  // 1. Initialize daemon client and process manager
  client = new DaemonClient(port);
  daemonProcess = new DaemonProcess(client, port);

  // 2. Initialize status bar
  statusBar = new StatusBarManager();
  context.subscriptions.push({ dispose: () => statusBar.dispose() });

  // 3. Try to connect
  const connected = await daemonProcess.ensureRunning();
  statusBar.update(connected);

  if (connected) {
    try {
      const models = await client.listModels();
      const defaultModel = models.find(m => m.is_default);
      if (defaultModel) statusBar.update(true, defaultModel.model);
    } catch {
      // Model fetch failed, keep connected status without model name
    }
  }

  // 4. Register ChatViewProvider
  chatProvider = new ChatViewProvider(context.extensionUri, client);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewType, chatProvider, {
      webviewOptions: { retainContextWhenHidden: true },
    })
  );

  // 5. Register diff content provider
  const diffProvider = new DiffContentProvider();
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider('atomcode-original', diffProvider)
  );

  // 6. Register CodeAction provider (for all languages)
  context.subscriptions.push(
    vscode.languages.registerCodeActionsProvider('*', new AtomCodeActionProvider(), {
      providedCodeActionKinds: AtomCodeActionProvider.providedCodeActionKinds,
    })
  );

  // 7. Register commands
  context.subscriptions.push(
    vscode.commands.registerCommand('atomcode.openSidebar', () => {
      vscode.commands.executeCommand('atomcode.chatView.focus');
    }),

    vscode.commands.registerCommand('atomcode.openTab', () => {
      // Open chat in a new editor tab (future: separate webview panel)
      vscode.commands.executeCommand('atomcode.chatView.focus');
    }),

    vscode.commands.registerCommand('atomcode.focusInput', () => {
      chatProvider.focusInput();
    }),

    vscode.commands.registerCommand('atomcode.newConversation', () => {
      chatProvider.newConversation();
    }),

    vscode.commands.registerCommand('atomcode.stop', () => {
      chatProvider.stopGeneration();
    }),

    vscode.commands.registerCommand('atomcode.explain', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please explain this code. What does it do, and why?', ctx);
      chatProvider.sendMessage(prompt);
    }),

    vscode.commands.registerCommand('atomcode.fix', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please fix any bugs or issues in this code.', ctx);
      chatProvider.sendMessage(prompt);
    }),

    vscode.commands.registerCommand('atomcode.optimize', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please optimize this code for better performance and readability.', ctx);
      chatProvider.sendMessage(prompt);
    }),
  );

  // 8. Periodic health check (every 30s)
  healthCheckInterval = setInterval(async () => {
    const isRunning = await client.isRunning();
    statusBar.update(isRunning, undefined, undefined);
  }, 30000);

  // 9. Listen for config changes
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('atomcode')) {
        const newConfig = vscode.workspace.getConfiguration('atomcode');
        const newPort = newConfig.get<number>('daemon.port', 23462);
        if (newPort !== port) {
          vscode.window.showInformationMessage('AtomCode: Restart VS Code to apply port change.');
        }
      }
    })
  );
}

export function deactivate() {
  if (healthCheckInterval) clearInterval(healthCheckInterval);
  daemonProcess?.dispose();
}
