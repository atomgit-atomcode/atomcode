import * as vscode from 'vscode';
import { DaemonClient } from './daemon/client';
import { DaemonProcess } from './daemon/process';
import { ChatViewProvider } from './chat/provider';
import { StatusBarManager } from './status';
import { AtomCodeActionProvider } from './editor/actions';
import { DiffContentProvider } from './editor/diff';
import { getEditorContext, buildContextualPrompt } from './editor/context';
import { getConfig, DEFAULT_PORT } from './config';

class ExtensionState {
  client!: DaemonClient;
  daemonProcess!: DaemonProcess;
  chatProvider!: ChatViewProvider;
  statusBar!: StatusBarManager;
  healthCheckInterval: ReturnType<typeof setInterval> | null = null;
}

const extensionState = new ExtensionState();

export async function activate(context: vscode.ExtensionContext) {
  const config = getConfig();

  // 1. Initialize daemon client and process manager
  extensionState.client = new DaemonClient(config.daemonPort);
  extensionState.daemonProcess = new DaemonProcess(extensionState.client, context.extensionUri, {
    defaultPort: config.daemonPort,
    binaryPath: config.binaryPath,
    autoStart: config.autoStart,
  });

  // 2. Initialize status bar
  extensionState.statusBar = new StatusBarManager();
  context.subscriptions.push({ dispose: () => extensionState.statusBar.dispose() });

  // 3. Try to connect
  const connected = await extensionState.daemonProcess.ensureRunning();
  extensionState.statusBar.update(connected);

  if (connected) {
    try {
      const models = await extensionState.client.listModels();
      const defaultModel = models.find(m => m.is_default);
      if (defaultModel) extensionState.statusBar.update(true, defaultModel.model);
    } catch {
      // Model fetch failed, keep connected status without model name
    }
  }

  // 4. Register ChatViewProvider
  extensionState.chatProvider = new ChatViewProvider(context.extensionUri, extensionState.client);
  context.subscriptions.push({ dispose: () => extensionState.chatProvider.dispose() });
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewType, extensionState.chatProvider, {
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
  const cmds = [
    vscode.commands.registerCommand('atomcode.openSidebar', () => {
      vscode.commands.executeCommand('atomcode.chatView.focus');
    }),

    vscode.commands.registerCommand('atomcode.openTab', () => {
      extensionState.chatProvider.openInTab();
    }),

    vscode.commands.registerCommand('atomcode.focusInput', () => {
      extensionState.chatProvider.focusInput();
    }),

    vscode.commands.registerCommand('atomcode.newConversation', () => {
      extensionState.chatProvider.newConversation();
    }),

    vscode.commands.registerCommand('atomcode.stop', () => {
      extensionState.chatProvider.stopGeneration();
    }),

    vscode.commands.registerCommand('atomcode.explain', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please explain this code. What does it do, and why?', ctx);
      extensionState.chatProvider.sendMessage(prompt);
    }),

    vscode.commands.registerCommand('atomcode.fix', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please fix any bugs or issues in this code.', ctx);
      extensionState.chatProvider.sendMessage(prompt);
    }),

    vscode.commands.registerCommand('atomcode.optimize', () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt('Please optimize this code for better performance and readability.', ctx);
      extensionState.chatProvider.sendMessage(prompt);
    }),
  ];
  context.subscriptions.push(...cmds);

  // 8. Wire model selection to status bar
  extensionState.chatProvider.onModelSelected = (model: string) => {
    extensionState.statusBar.update(true, model);
  };

  // 9. Listen for active editor changes → send context to webview
  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(() => {
      extensionState.chatProvider.sendEditorContext();
    }),
    vscode.window.onDidChangeTextEditorSelection(() => {
      extensionState.chatProvider.sendEditorContext();
    })
  );

  // 10. Periodic health check (every 30s)
  extensionState.healthCheckInterval = setInterval(async () => {
    const isRunning = await extensionState.client.isRunning();
    extensionState.statusBar.update(isRunning, undefined, undefined);
  }, 30000);

  // 11. Listen for config changes
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('atomcode')) {
        const newConfig = vscode.workspace.getConfiguration('atomcode');
        const newPort = newConfig.get<number>('daemon.port', 13456);
        if (newPort !== config.daemonPort) {
          vscode.window.showInformationMessage('AtomCode: Restart VS Code to apply port change.');
        }
      }
    })
  );
}

export function deactivate() {
  if (extensionState.healthCheckInterval) {
    clearInterval(extensionState.healthCheckInterval);
    extensionState.healthCheckInterval = null;
  }
  extensionState.daemonProcess.dispose();
}
