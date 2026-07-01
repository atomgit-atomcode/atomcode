import * as vscode from 'vscode';
import { DaemonClient } from './daemon/client';
import { DaemonProcess } from './daemon/process';
import { ChatViewProvider } from './chat/provider';
import { StatusBarManager } from './status';
import { AtomCodeActionProvider } from './editor/actions';
import { DiffContentProvider } from './editor/diff';
import { getEditorContext, buildContextualPrompt } from './editor/context';
import { getConfig, DEFAULT_PORT } from './config';
import { getQuickActionPrompt } from './chat/quickActions';

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

  // Wire auto-reconnect: if daemon exits (idle timeout), restart it on next request.
  // Use a shared promise to prevent concurrent reconnection attempts from spawning
  // multiple daemon processes.
  let reconnecting: Promise<boolean> | null = null;
  extensionState.client.onConnectionLost = async () => {
    if (!reconnecting) {
      reconnecting = extensionState.daemonProcess.ensureRunning().finally(() => { reconnecting = null; });
    }
    return reconnecting;
  };

  // 2. Initialize status bar
  extensionState.statusBar = new StatusBarManager();
  context.subscriptions.push({ dispose: () => extensionState.statusBar.dispose() });

  // 3. Register ChatViewProvider before daemon startup so commands are never blocked by daemon readiness.
  extensionState.chatProvider = new ChatViewProvider(context.extensionUri, extensionState.client);
  context.subscriptions.push({ dispose: () => extensionState.chatProvider.dispose() });
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewType, extensionState.chatProvider, {
      webviewOptions: { retainContextWhenHidden: true },
    })
  );

  // 4. Register diff content provider
  const diffProvider = new DiffContentProvider();
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider('atomcode-original', diffProvider)
  );

  // 5. Register CodeAction provider (for all languages)
  context.subscriptions.push(
    vscode.languages.registerCodeActionsProvider('*', new AtomCodeActionProvider(), {
      providedCodeActionKinds: AtomCodeActionProvider.providedCodeActionKinds,
    })
  );

  // 6. Register commands before daemon startup. Command handlers surface daemon errors in the chat UI.
  const cmds = [
    vscode.commands.registerCommand('atomcode.openSidebar', async () => {
      await runCommand(vscode.l10n.t('open AtomCode sidebar'), () => extensionState.chatProvider.openInSidebar());
    }),

    vscode.commands.registerCommand('atomcode.openTab', () => {
      extensionState.chatProvider.openInTab();
    }),

    vscode.commands.registerCommand('atomcode.openPreferredLocation', async () => {
      await runCommand(vscode.l10n.t('open AtomCode'), () => extensionState.chatProvider.openPreferredLocation());
    }),

    vscode.commands.registerCommand('atomcode.focusInput', async () => {
      await runCommand(vscode.l10n.t('focus AtomCode input'), () => extensionState.chatProvider.focusInput());
    }),

    vscode.commands.registerCommand('atomcode.newConversation', async () => {
      await runCommand(vscode.l10n.t('start a new AtomCode conversation'), () => extensionState.chatProvider.newConversation());
    }),

    vscode.commands.registerCommand('atomcode.stop', () => {
      extensionState.chatProvider.stopGeneration();
    }),

    vscode.commands.registerCommand('atomcode.explain', async () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt(getQuickActionPrompt('explain', vscode.env.language), ctx, vscode.env.language);
      await runCommand(vscode.l10n.t('explain the selected code'), () => extensionState.chatProvider.sendEditorCommandMessage(prompt));
    }),

    vscode.commands.registerCommand('atomcode.fix', async () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt(getQuickActionPrompt('fix', vscode.env.language), ctx, vscode.env.language);
      await runCommand(vscode.l10n.t('fix the selected code'), () => extensionState.chatProvider.sendEditorCommandMessage(prompt));
    }),

    vscode.commands.registerCommand('atomcode.optimize', async () => {
      const ctx = getEditorContext();
      const prompt = buildContextualPrompt(getQuickActionPrompt('optimize', vscode.env.language), ctx, vscode.env.language);
      await runCommand(vscode.l10n.t('optimize the selected code'), () => extensionState.chatProvider.sendEditorCommandMessage(prompt));
    }),

    vscode.commands.registerCommand('atomcode.addToChat', async () => {
      const ctx = getEditorContext();
      if (!ctx.selection || !ctx.filePath) return;
      await runCommand(vscode.l10n.t('add selection to chat'), () => extensionState.chatProvider.addToChat({
        path: ctx.filePath!,
        fileName: ctx.fileName!,
        language: ctx.language,
        selection: ctx.selection,
        startLine: ctx.startLine,
        endLine: ctx.endLine,
      }));
    }),
  ];
  context.subscriptions.push(...cmds);

  // Register panel serializer for cross-restart tab restoration
  context.subscriptions.push(
    vscode.window.registerWebviewPanelSerializer('atomcode.chatTab', {
      async deserializeWebviewPanel(panel: vscode.WebviewPanel, state: any) {
        const sessionId = state?.sessionId as string | undefined;
        const projectHash = state?.projectHash as string | undefined;
        extensionState.chatProvider.setupPanelForRestore(panel, sessionId, projectHash);
      },
    })
  );

  // Register openSessionInTab command (called from webview)
  context.subscriptions.push(
    vscode.commands.registerCommand('atomcode.openSessionInTab', async (sessionId?: string, projectHash?: string) => {
      await extensionState.chatProvider.openSessionInTab(sessionId, projectHash);
    })
  );

  // 7. Try to connect in the background. Activation must stay responsive for menu commands.
  void initializeDaemon();

  // 8. Wire model selection to status bar
  extensionState.chatProvider.onModelSelected = (model: string) => {
    extensionState.statusBar.update(true, model);
  };

  // 9. Listen for active editor changes → send context to webview (only on editor switch, not selection changes)
  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(() => {
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
          vscode.window.showInformationMessage(vscode.l10n.t('AtomCode: Restart VS Code to apply port change.'));
        }
      }
    })
  );
}

async function runCommand(label: string, command: () => Thenable<unknown> | Promise<unknown> | unknown) {
  try {
    await command();
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    vscode.window.showErrorMessage(vscode.l10n.t('AtomCode failed to {label}: {message}', { label, message }));
  }
}

async function initializeDaemon() {
  try {
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
  } catch (e) {
    extensionState.statusBar.update(false);
    const message = e instanceof Error ? e.message : String(e);
    vscode.window.showWarningMessage(vscode.l10n.t('AtomCode daemon startup failed: {message}', { message }));
  }
}

export function deactivate() {
  if (extensionState.healthCheckInterval) {
    clearInterval(extensionState.healthCheckInterval);
    extensionState.healthCheckInterval = null;
  }
  extensionState.daemonProcess.dispose();
}
