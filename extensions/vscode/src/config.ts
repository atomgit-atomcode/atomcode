import * as vscode from 'vscode';

// Centralized default values
const DEFAULT_PORT = 13456;
const AUTO_START_DEFAULT = true;
const BINARY_PATH_DEFAULT = '';
const PREFERRED_LOCATION_DEFAULT = 'sidebar';
const AUTO_SAVE_DEFAULT = true;
const SEND_WITH_CTRL_ENTER_DEFAULT = false;
const FONT_SIZE_DEFAULT = 13;
const SHOW_INLINE_HINTS_DEFAULT = true;

export { DEFAULT_PORT };

export function getConfig() {
  const config = vscode.workspace.getConfiguration('atomcode');
  return {
    daemonPort: config.get<number>('daemon.port', DEFAULT_PORT),
    autoStart: config.get<boolean>('daemon.autoStart', AUTO_START_DEFAULT),
    binaryPath: config.get<string>('daemon.binaryPath', BINARY_PATH_DEFAULT),
    preferredLocation: config.get<string>('preferredLocation', PREFERRED_LOCATION_DEFAULT),
    autoSave: config.get<boolean>('autoSave', AUTO_SAVE_DEFAULT),
    sendWithCtrlEnter: config.get<boolean>('sendWithCtrlEnter', SEND_WITH_CTRL_ENTER_DEFAULT),
    fontSize: config.get<number>('fontSize', FONT_SIZE_DEFAULT),
    showInlineHints: config.get<boolean>('showInlineHints', SHOW_INLINE_HINTS_DEFAULT),
  };
}

/** Read a single config value with a typed default. */
export function getConf<T>(section: string, defaultValue: T): T {
  return vscode.workspace.getConfiguration('atomcode').get<T>(section, defaultValue);
}
