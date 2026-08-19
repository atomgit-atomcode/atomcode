import assert from 'node:assert/strict';
import Module from 'node:module';

// Minimal vscode mock so provider.ts can be loaded in a Node test context.
const originalLoad = (Module as unknown as { _load: typeof Module['_load'] })._load;
(Module as unknown as { _load: typeof Module['_load'] })._load = function (request: string, parent: unknown, isMain: boolean) {
  if (request === 'vscode') {
    return {
      Uri: { joinPath: (...parts: Array<{ fsPath?: string } | string>) => ({ fsPath: parts.map((p) => typeof p === 'string' ? p : p.fsPath || '').join('/') }) },
      RelativePattern: class {},
      workspace: { workspaceFolders: [], onDidChangeConfiguration: () => ({ dispose() {} }), getConfiguration: () => ({ get: () => undefined }), createFileSystemWatcher: () => ({ dispose() {} }), onDidSaveTextDocument: () => ({ dispose() {} }), onDidChangeTextDocument: () => ({ dispose() {} }), onDidCloseTextDocument: () => ({ dispose() {} }), onDidOpenTextDocument: () => ({ dispose() {} }), onDidChangeWorkspaceFolders: () => ({ dispose() {} }) },
      window: { activeTextEditor: undefined, showInformationMessage: async () => undefined, showErrorMessage: async () => undefined, showInputBox: async () => undefined, createWebviewPanel: () => ({ webview: { html: '', onDidReceiveMessage: () => ({ dispose() {} }), postMessage: () => true, asWebviewUri: (u: unknown) => u, cspSource: '' }, onDidDispose: () => ({ dispose() {} }), onDidChangeViewState: () => ({ dispose() {} }), reveal: () => {}, dispose: () => {}, title: '', iconPath: undefined }), createOutputChannel: () => ({ appendLine() {}, append() {}, show() {}, dispose() {} }), registerWebviewViewProvider: () => ({ dispose() {} }), registerCommand: () => ({ dispose() {} }) },
      commands: { registerCommand: () => ({ dispose() {} }), executeCommand: async () => undefined },
      ViewColumn: { Beside: 2, One: 1, Two: 2, Three: 3, Active: -1 },
      StatusBarAlignment: { Left: 1, Right: 2 },
      env: { machineId: 'test', uriScheme: 'vscode', openExternal: async () => true },
      l10n: { t: (s: string) => s },
      ExtensionMode: { Test: 1, Development: 2, Production: 3 },
      EventEmitter: class { constructor() { this.event = () => ({ dispose() {} }); } fire() {} dispose() {} },
      Disposable: { from: () => ({ dispose() {} }) },
      ThemeIcon: class { constructor(public id: string) {} },
      TreeItem: class { constructor(public label: string) {} },
      TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
    };
  }
  return originalLoad.call(this, request, parent, isMain);
};

// Import after mock is installed.
const { truncateTabTitle } = require('../../src/chat/provider');

function testShortLatinNotTruncated() {
  const input = 'Hello world';
  assert.equal(truncateTabTitle(input), input);
}

function testLongLatinTruncated() {
  const input = 'This is a very long session name that exceeds the tab title width limit of thirty visual units';
  const result = truncateTabTitle(input);
  assert.ok(result.endsWith('\u2026'), `expected ellipsis, got: ${result}`);
  assert.ok(result.length < input.length, 'result should be shorter than input');
}

function testCJKTruncatedEarlier() {
  // Each CJK char has visual width 2, so 15 Chinese chars already reach width 30.
  const cjk = '这是一个非常长的中文会话名称用来测试标签页标题截断功能是否正常工作';
  const result = truncateTabTitle(cjk);
  assert.ok(result.endsWith('\u2026'), `expected ellipsis, got: ${result}`);
  // 15 CJK chars * 2 = 30, next char would exceed → truncate at 15 chars + ellipsis
  const visible = result.slice(0, -1);
  assert.equal(visible.length, 15, `expected exactly 15 CJK chars before ellipsis, got ${visible.length}: ${visible}`);
}

function testMixedWidthTruncation() {
  // "abc" = width 3, then each CJK char = width 2. After abc + 13 CJK = 29, next CJK exceeds 30.
  const input = 'abc一二三四五六七八九十一二三四五';
  const result = truncateTabTitle(input);
  assert.ok(result.endsWith('\u2026'), `expected ellipsis, got: ${result}`);
}

function testEmojiDoesNotBreak() {
  // 35 emoji code points: each has visual width 1, so 35 > 30 triggers truncation.
  // Emoji use surrogate pairs in UTF-16; Array.from ensures they are not split.
  const emoji = '\u{1F389}'; // 🎉
  const input = emoji.repeat(35);
  const result = truncateTabTitle(input);
  assert.ok(result.endsWith('\u2026'), `expected ellipsis, got: ${result}`);
  // No replacement character / broken surrogate
  assert.ok(!result.includes('\ufffd'), 'should not contain replacement character');
  // Result should be composed of whole emoji characters + ellipsis (no half-emoji)
  const visible = result.slice(0, -1);
  for (const ch of Array.from(visible)) {
    assert.equal(ch, emoji, `expected only whole emoji, got U+${ch.codePointAt(0)?.toString(16)}`);
  }
}

function testNewlinesAndTabsAreCleaned() {
  const input = 'Session\r\nwith\nline\tbreaks';
  const result = truncateTabTitle(input);
  assert.ok(!/\r|\n|\t/.test(result), `should not contain control chars, got: ${JSON.stringify(result)}`);
  assert.ok(result.includes('with line breaks'));
}

function testEmptyString() {
  assert.equal(truncateTabTitle(''), '');
}

function testWhitespaceOnly() {
  assert.equal(truncateTabTitle('   '), '');
}

function testLeadingAndTrailingWhitespaceTrimmed() {
  assert.equal(truncateTabTitle('  hello  '), 'hello');
  assert.equal(truncateTabTitle('\t\thello\t\t'), 'hello');
}

function testExactlyAtWidthLimit() {
  // 30 ASCII chars = exactly width 30, no truncation
  const input = 'a'.repeat(30);
  assert.equal(truncateTabTitle(input), input);
}

function testOneOverWidthLimit() {
  // 31 ASCII chars = width 31, should be truncated to 30 + ellipsis
  const input = 'a'.repeat(31);
  const result = truncateTabTitle(input);
  assert.equal(result, 'a'.repeat(30) + '\u2026');
}

const tests = [
  testShortLatinNotTruncated,
  testLongLatinTruncated,
  testCJKTruncatedEarlier,
  testMixedWidthTruncation,
  testEmojiDoesNotBreak,
  testNewlinesAndTabsAreCleaned,
  testEmptyString,
  testWhitespaceOnly,
  testLeadingAndTrailingWhitespaceTrimmed,
  testExactlyAtWidthLimit,
  testOneOverWidthLimit,
];

let passed = 0;
for (const fn of tests) {
  try {
    fn();
    passed++;
    console.log(`  ✓ ${fn.name}`);
  } catch (e) {
    console.error(`  ✗ ${fn.name}`);
    console.error(e);
    process.exit(1);
  }
}
console.log(`\n${passed}/${tests.length} passed`);
