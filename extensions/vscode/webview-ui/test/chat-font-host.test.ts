import assert from 'node:assert/strict';
import Module from 'node:module';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

let value: unknown = 20;
let watcher: (event: { affectsConfiguration: (key: string) => boolean }) => void;
let windowStateCb: ((state: { focused: boolean }) => void) | undefined;
const original = (Module as any)._load;
(Module as any)._load = function(request: string, ...args: any[]) {
  if (request === 'vscode') return {
    workspace: {
      onDidChangeConfiguration: (cb: typeof watcher) => { watcher = cb; return { dispose() {} }; },
      getConfiguration: () => ({ get: (key: string, fallback: unknown) => key === 'fontSize' ? value : fallback }),
    },
    Uri: { joinPath: (uri: { fsPath: string }, ...parts: string[]) => ({ fsPath: join(uri.fsPath, ...parts) }) },
    env: { language: 'en' },
    window: { onDidChangeWindowState: (cb: (state: { focused: boolean }) => void) => { windowStateCb = cb; return { dispose() {} }; } },
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

// Covers the window-state focus restore branch: the webview reports whether the
// chat input held focus (chatInputFocusState), and the host hands focus back to
// that webview when the OS window regains focus on Windows.
async function runFocusRestoreTests(provider: any) {
  const sent: unknown[] = [];
  let messageHandler: (msg: unknown) => Promise<void> = async () => {};
  const fakeWebview = {
    postMessage: (m: unknown) => { sent.push(m); },
    onDidReceiveMessage: (cb: (msg: unknown) => Promise<void>) => { messageHandler = cb; return { dispose() {} }; },
  } as any;
  provider._setupWebviewMessageHandler(fakeWebview, 'sidebar');
  const focusInputPosts = () => sent.filter(m => (m as any).type === 'focusInput').length;
  const waitRestoreTimer = () => new Promise(r => setTimeout(r, 150));

  // 1. Input held focus on window blur → restore is posted to the reporting webview.
  windowStateCb!({ focused: false });
  await messageHandler({ type: 'chatInputFocusState', focused: true });
  windowStateCb!({ focused: true });
  await waitRestoreTimer();
  assert.equal(focusInputPosts(), 1);

  // 2. Flag cleared after restore → a second window cycle does not re-post.
  windowStateCb!({ focused: false });
  windowStateCb!({ focused: true });
  await waitRestoreTimer();
  assert.equal(focusInputPosts(), 1);

  // 3. Input did not hold focus on blur → no restore.
  windowStateCb!({ focused: false });
  await messageHandler({ type: 'chatInputFocusState', focused: false });
  windowStateCb!({ focused: true });
  await waitRestoreTimer();
  assert.equal(focusInputPosts(), 1);

  // 4. Blur report far older than the window blur (focus moved in-app) → no restore.
  windowStateCb!({ focused: false });
  await messageHandler({ type: 'chatInputFocusState', focused: true });
  provider._chatInputBlurAt = Date.now() - 5000;
  windowStateCb!({ focused: true });
  await waitRestoreTimer();
  assert.equal(focusInputPosts(), 1);

  // 5. Not Windows → the restore branch stays off entirely.
  const realPlatform = process.platform;
  Object.defineProperty(process, 'platform', { value: 'darwin' });
  try {
    windowStateCb!({ focused: false });
    await messageHandler({ type: 'chatInputFocusState', focused: true });
    windowStateCb!({ focused: true });
    await waitRestoreTimer();
    assert.equal(focusInputPosts(), 1);
  } finally {
    Object.defineProperty(process, 'platform', { value: realPlatform });
  }
}

main().catch((err) => { console.error(err); process.exit(1); });
