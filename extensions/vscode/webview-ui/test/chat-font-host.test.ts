import assert from 'node:assert/strict';
import Module from 'node:module';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

let value: unknown = 20;
let watcher: (event: { affectsConfiguration: (key: string) => boolean }) => void;
const original = (Module as any)._load;
(Module as any)._load = function(request: string, ...args: any[]) {
  if (request === 'vscode') return {
    workspace: {
      onDidChangeConfiguration: (cb: typeof watcher) => { watcher = cb; return { dispose() {} }; },
      getConfiguration: () => ({ get: (key: string, fallback: unknown) => key === 'fontSize' ? value : fallback }),
    },
    Uri: { joinPath: (uri: { fsPath: string }, ...parts: string[]) => ({ fsPath: join(uri.fsPath, ...parts) }) },
    env: { language: 'en' },
  };
  return original.call(this, request, ...args);
};
const { ChatViewProvider, resolveChatFontSize } = require('../../src/chat/provider');
(Module as any)._load = original;
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
} finally { rmSync(directory, { recursive: true, force: true }); }
console.log('chat font host: initial HTML, live sidebar/tab broadcast and bounds passed');
