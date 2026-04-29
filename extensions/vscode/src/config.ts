import * as vscode from 'vscode';

export function getConfig() {
  const config = vscode.workspace.getConfiguration('atomcode');
  return {
    daemonPort: config.get<number>('daemon.port', 23462),
    autoStart: config.get<boolean>('daemon.autoStart', true),
    binaryPath: config.get<string>('daemon.binaryPath', ''),
    preferredLocation: config.get<string>('preferredLocation', 'sidebar'),
    autoSave: config.get<boolean>('autoSave', true),
    sendWithCtrlEnter: config.get<boolean>('sendWithCtrlEnter', false),
    fontSize: config.get<number>('fontSize', 13),
    showInlineHints: config.get<boolean>('showInlineHints', true),
  };
}
