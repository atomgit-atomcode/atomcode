import assert from 'node:assert/strict';
import Module from 'node:module';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

let value: unknown = 20;
let watcher: (event: { affectsConfiguration: (key: string) => boolean }) => void;
let windowStateCb: ((state: { focused: boolean }) => void) | undefined;
const windowApi = {
  activeTextEditor: undefined as object | undefined,
  onDidChangeWindowState(callback: (state: { focused: boolean }) => void) {
    windowStateCb = callback;
    return { dispose() {} };
  },
};
const original = (Module as any)._load;
(Module as any)._load = function(request: string, ...args: any[]) {
  if (request === 'vscode') return {
    workspace: {
      onDidChangeConfiguration: (cb: typeof watcher) => { watcher = cb; return { dispose() {} }; },
      getConfiguration: () => ({ get: (key: string, fallback: unknown) => key === 'fontSize' ? value : fallback }),
    },
    Uri: { joinPath: (uri: { fsPath: string }, ...parts: string[]) => ({ fsPath: join(uri.fsPath, ...parts) }) },
    env: { language: 'en' },
    window: windowApi,
  };
  return original.call(this, request, ...args);
};
const { ChatViewProvider, resolveChatFontSize } = require('../../src/chat/provider');
(Module as any)._load = original;

async function main() {
  const directory = mkdtempSync(join(tmpdir(), 'atomcode-font-host-'));
  try {
    mkdirSync(join(directory, 'webview'));
    writeFileSync(join(directory, 'webview/index.html'), readFileSync(join(process.cwd(), 'webview/index.html')));
    for (const file of ['webview.js', 'webview.css']) writeFileSync(join(directory, 'webview', file), '');
    const messages: unknown[] = [];
    const tabMessages: unknown[] = [];
    const provider: any = new ChatViewProvider({ fsPath: directory }, {});
    provider._view = { webview: { postMessage: (m: unknown) => messages.push(m) } };
    provider._webviewPanels.set({ postMessage: (m: unknown) => tabMessages.push(m) }, {});
    watcher!({ affectsConfiguration: key => key === 'atomcode.fontSize' });
    assert.deepEqual(messages, [{ type: 'chromeFontSize', value: 20 }]);
    assert.deepEqual(tabMessages, messages);
    const html = provider._getHtml({ asWebviewUri: () => ({ toString: () => 'https://example.invalid/asset' }), cspSource: 'self' }, 'tab');
    assert.ok(html.includes('data-font-size="20"'));
    assert.ok(html.includes('--app-font-size: 20px;'));
    for (const [input, expected] of [[100, 40], [-1, 9], [NaN, 13], ['20', 13], [15.5, 16]]) {
      value = input;
      assert.equal(resolveChatFontSize(), expected);
    }
    await runFocusRestoreTests(provider);
  } finally { rmSync(directory, { recursive: true, force: true }); }
  console.log('chat font host: initial HTML, live sidebar/tab broadcast and bounds passed');
}

async function runFocusRestoreTests(provider: any) {
  const sent: unknown[] = [];
  let messageHandler: (msg: unknown) => Promise<void> = async () => {};
  const webview = {
    postMessage: (message: unknown) => { sent.push(message); },
    onDidReceiveMessage: (callback: typeof messageHandler) => {
      messageHandler = callback;
      return { dispose() {} };
    },
  };
  const sidebar = { webview, visible: true };
  provider._view = sidebar;
  provider._setupWebviewMessageHandler(webview, 'sidebar');
  const posts = () => sent.filter(message => (message as { type: string }).type === 'restoreChatInput').length;
  const settle = () => new Promise(resolve => setTimeout(resolve, 150));
  const windowFocus = (focused: boolean) => windowStateCb!({ focused });
  const report = (windowsClient = true, focused = true, altHeld = false) =>
    messageHandler({ type: 'chatInputFocusState', windowsClient, focused, altHeld });

  // The browser identifies a Windows client independently of the Node host.
  const platform = Object.getOwnPropertyDescriptor(process, 'platform')!;
  Object.defineProperty(process, 'platform', { value: 'linux' });
  try {
    windowFocus(false);
    await report();
    windowFocus(true);
    await settle();
    assert.equal(posts(), 1, 'a Windows client can restore through a remote Linux host');

    await report(true, true, true);
    windowFocus(false);
    windowFocus(true);
    await settle();
    assert.equal(posts(), 2, 'a held Alt can identify a webview report arriving first');
  } finally {
    Object.defineProperty(process, 'platform', platform);
  }

  await report();
  windowFocus(false);
  windowFocus(true);
  await settle();
  assert.equal(posts(), 2, 'an in-app blur while the VS Code window was focused is ignored');

  windowFocus(false);
  await report(false);
  windowFocus(true);
  await settle();
  assert.equal(posts(), 2, 'non-Windows clients never restore focus');

  windowFocus(false);
  await report();
  await report(true, false);
  windowFocus(true);
  await settle();
  assert.equal(posts(), 2, 'a newer report that the input was not focused cancels recovery');

  windowFocus(false);
  await report();
  windowFocus(true);
  windowFocus(false);
  await settle();
  assert.equal(posts(), 2, 'another OS blur cancels a scheduled restore');
  windowFocus(true);

  windowFocus(false);
  await report();
  windowFocus(true);
  sidebar.visible = false;
  await settle();
  assert.equal(posts(), 2, 'a hidden view does not receive focus');

  sidebar.visible = true;
  windowFocus(false);
  await report();
  windowFocus(true);
  windowApi.activeTextEditor = {};
  await settle();
  assert.equal(posts(), 2, 'a newly selected editor keeps focus during the restore delay');
}

main().catch((err) => { console.error(err); process.exit(1); });
