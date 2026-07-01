import assert from 'node:assert/strict';
import Module from 'node:module';

declare const require: {
  (id: string): typeof import('../../src/editor/context');
};

const originalLoad = (Module as unknown as { _load: typeof Module['_load'] })._load;
(Module as unknown as { _load: typeof Module['_load'] })._load = function patchedLoad(request, parent, isMain) {
  if (request === 'vscode') {
    return { window: {} };
  }
  return originalLoad.call(this, request, parent, isMain);
};

const { buildContextualPrompt } = require('../../src/editor/context');

(Module as unknown as { _load: typeof Module['_load'] })._load = originalLoad;

function testContextualPromptUsesChineseLabelsForChineseLocale() {
  const prompt = buildContextualPrompt(
    '请解释这段代码。它做了什么，为什么这样做？',
    {
      fileName: 'provider.ts',
      language: 'typescript',
      selection: 'const value = 1;',
      startLine: 12,
      endLine: 12,
    },
    'zh-CN',
  );

  assert.match(prompt, /^文件：provider\.ts \(typescript\)\n选中代码（第 12 行）：/);
  assert.doesNotMatch(prompt, /^File:/);
  assert.doesNotMatch(prompt, /Selected code/);
}

function testContextualPromptKeepsEnglishLabelsForEnglishLocale() {
  const prompt = buildContextualPrompt(
    'Please explain this code. What does it do and why?',
    {
      fileName: 'provider.ts',
      language: 'typescript',
      selection: 'const value = 1;',
      startLine: 12,
      endLine: 14,
    },
    'en-US',
  );

  assert.match(prompt, /^File: provider\.ts \(typescript\)\nSelected code \(lines 12-14\):/);
}

testContextualPromptUsesChineseLabelsForChineseLocale();
testContextualPromptKeepsEnglishLabelsForEnglishLocale();
