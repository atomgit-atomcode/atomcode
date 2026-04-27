import * as vscode from 'vscode';
import { DaemonClient } from './daemon/client';
import { DaemonProcess } from './daemon/process';

let client: DaemonClient;
let daemonProcess: DaemonProcess;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const config = vscode.workspace.getConfiguration('atomcode');
  const port = config.get<number>('daemon.port', 23462);

  client = new DaemonClient(port);
  daemonProcess = new DaemonProcess(client, port);

  // Try to connect to daemon
  const running = await daemonProcess.ensureRunning();
  if (running) {
    vscode.window.showInformationMessage('AtomCode: Connected to daemon');
  }

  // Placeholder command registrations (will be expanded in next tasks)
  context.subscriptions.push(
    vscode.commands.registerCommand('atomcode.openSidebar', () => {
      vscode.window.showInformationMessage('AtomCode chat coming soon');
    })
  );
}

export function deactivate(): void {
  daemonProcess?.dispose();
}
