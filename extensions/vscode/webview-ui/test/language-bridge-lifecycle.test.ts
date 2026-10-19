import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import * as http from 'node:http';
import { atomcodeHome } from '../../src/paths';

async function main() {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'atomcode-bridge-lifecycle-'));
  const oldHome = process.env.ATOMCODE_HOME;
  process.env.ATOMCODE_HOME = root;
  const commands = new Map<string, (...args: unknown[]) => unknown>();
  const subscriptions: Array<{ dispose(): void }> = [];
  let foldersChanged: (() => void) | undefined;
  let clipboard = '';
  let nextChoice = '';
  let errors = 0;
  let trusted = true;
  const bar = { text: '', tooltip: '', command: '', show() {}, dispose() {} };
  const mock = {
    StatusBarAlignment: { Right: 1 },
    l10n: { t: (message: string, ...args: string[]) => message.replace(/\{(\d+)\}/gu, (_, i) => args[Number(i)] ?? '') },
    env: { appName: 'fixture IDE', clipboard: { writeText: async (text: string) => { clipboard = text; } } },
    workspace: {
      get isTrusted() { return trusted; },
      workspaceFolders: [{ uri: { scheme: 'file', fsPath: root } }],
      onDidChangeWorkspaceFolders: (callback: () => void) => { foldersChanged = callback; return { dispose() {} }; },
    },
    window: {
      createStatusBarItem: () => bar,
      showInformationMessage: async () => {},
      showErrorMessage: async () => { errors++; },
      showQuickPick: async (choices: Array<{ command: string }>) => choices.find(choice => choice.command === nextChoice),
    },
    commands: {
      registerCommand: (name: string, callback: (...args: unknown[]) => unknown) => {
        commands.set(name, callback); return { dispose() { commands.delete(name); } };
      },
      executeCommand: async (name: string) => commands.get(name)?.(),
    },
  };
  // Exercise the actual registration/lifecycle against the public API boundary;
  // no installed VS Code runtime or daemon process is needed.
  const module = require('node:module');
  const previousLoad = module._load;
  module._load = function(name: string, ...args: unknown[]) {
    return name === 'vscode' ? mock : previousLoad.call(this, name, ...args);
  };
  try {
    const { registerLanguageBridge } = await import('../../src/languageBridge/vscode');
    registerLanguageBridge({ subscriptions, globalStorageUri: { scheme: 'file', fsPath: root } } as never);
    assert.equal(atomcodeHome(), root);
    assert.match(bar.text, /off/u);
    const invoke = async (name: string) => { assert.ok(commands.has(name)); await commands.get(name)!(); };
    trusted = false;
    await invoke('atomcode.enableLanguageBridge');
    assert.equal(errors, 1);
    await assert.rejects(fs.access(path.join(root, 'ide-bridge')));
    trusted = true;
    await Promise.all([invoke('atomcode.enableLanguageBridge'), invoke('atomcode.enableLanguageBridge')]);
    assert.match(bar.text, /ready/u);
    assert.match(clipboard, /ide_discovery_dir = /u);
    const directory = path.join(root, 'ide-bridge');
    const files = await fs.readdir(directory);
    assert.equal(files.length, 1, 'concurrent enable must publish only one window');
    const connection = JSON.parse(await fs.readFile(path.join(directory, files[0]), 'utf8'));
    assert.ok(!clipboard.includes(connection.token));
    const health = await new Promise<Record<string, unknown>>((resolve, reject) => {
      const request = http.request(connection.endpoint.replace(/\/query$/u, '/status'), {
        method: 'POST', headers: { authorization: `Bearer ${connection.token}` },
      }, response => {
        const chunks: Buffer[] = [];
        response.on('data', chunk => chunks.push(chunk));
        response.on('end', () => resolve(JSON.parse(Buffer.concat(chunks).toString())));
      });
      request.once('error', reject); request.end();
    });
    assert.equal(health.status, 'ready');
    await invoke('atomcode.copyLanguageBridgeBinding');
    assert.match(clipboard, /ide_connection_file = /u);
    assert.ok(clipboard.includes(connection.window_id));
    assert.ok(!clipboard.includes(connection.token));
    nextChoice = 'atomcode.disableLanguageBridge';
    await invoke('atomcode.languageBridgeStatus');
    assert.match(bar.text, /off/u);
    assert.deepEqual(await fs.readdir(directory), []);
    await invoke('atomcode.enableLanguageBridge');
    foldersChanged!();
    // stop resets UI synchronously, then removes the private file asynchronously.
    assert.match(bar.text, /off/u);
    await invoke('atomcode.disableLanguageBridge');
    // Await the file removal triggered by workspace change rather than assuming
    // that a second, idempotent stop owns the preceding asynchronous cleanup.
    for (let attempt = 0; attempt < 20 && (await fs.readdir(directory)).length; attempt++) {
      await new Promise(resolve => setImmediate(resolve));
    }
    assert.deepEqual(await fs.readdir(directory), []);
    await Promise.all([invoke('atomcode.enableLanguageBridge'), invoke('atomcode.disableLanguageBridge')]);
    assert.deepEqual(await fs.readdir(directory), []);
    assert.match(bar.text, /off/u);
  } finally {
    module._load = previousLoad;
    for (const item of subscriptions.reverse()) item.dispose();
    if (oldHome === undefined) delete process.env.ATOMCODE_HOME;
    else process.env.ATOMCODE_HOME = oldHome;
    await fs.rm(root, { recursive: true, force: true });
  }
}
void main().catch(error => { console.error(error); process.exitCode = 1; });
