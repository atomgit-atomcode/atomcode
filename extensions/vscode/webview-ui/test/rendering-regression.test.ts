import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { marked } from 'marked';
import {
  classifyArtifactRenderKind,
  normalizeCodeArtifactContent,
  normalizeMarkdownArtifactContent,
  shouldRenderArtifactChrome,
} from '../src/components/artifactRendering';
import { renderCodeBlockHtml } from '../src/components/codeBlockRendering';
import { parseDiff } from '../src/components/DiffView';
import { prepareMarkdownForRender, repairStreamingMarkdown } from '../src/components/streamingMarkdown';

declare const require: {
  (id: string): typeof import('../src/state/reducer');
};

(globalThis as unknown as { document: { body: { dataset: { viewMode: string } } } }).document = {
  body: { dataset: { viewMode: 'sidebar' } },
};

const { chatReducer, initialState } = require('../src/state/reducer');

function renderMarkdownForTest(markdown: string): string {
  return marked.parse(markdown, { async: false }) as string;
}

function startAssistantState() {
  return chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, { type: 'START_GENERATION' });
}

function testStreamingBlocksPreserveTextArtifactTextOrder() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'before\n' });
  state = chatReducer(state, { type: 'ARTIFACT_START', id: 'artifact-1', artifactType: 'code', language: 'ts', title: 'src/types.ts' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-1', content: 'export interface ArtifactData {}' });
  state = chatReducer(state, { type: 'ARTIFACT_END', id: 'artifact-1' });
  state = chatReducer(state, { type: 'APPEND_TEXT', content: '\nafter' });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['text', 'artifact', 'text']);
  assert.equal(message.blocks?.[0].type === 'text' ? message.blocks[0].content : undefined, 'before\n');
  assert.equal(message.blocks?.[1].type === 'artifact' ? message.blocks[1].artifact.content : undefined, 'export interface ArtifactData {}');
  assert.equal(message.blocks?.[2].type === 'text' ? message.blocks[2].content : undefined, '\nafter');
}

function testArtifactContentBeforeStartKeepsBlockPosition() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'before\n' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-early', content: '+ first line' });
  state = chatReducer(state, { type: 'APPEND_TEXT', content: '\nafter\n' });
  state = chatReducer(state, { type: 'ARTIFACT_START', id: 'artifact-early', artifactType: 'code', language: 'diff', title: 'changes.diff' });
  state = chatReducer(state, { type: 'ARTIFACT_END', id: 'artifact-early' });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['text', 'artifact', 'text']);
  assert.equal(message.blocks?.[1].type === 'artifact' ? message.blocks[1].artifact.content : undefined, '+ first line');
  assert.equal(message.blocks?.[1].type === 'artifact' ? message.blocks[1].artifact.language : undefined, 'diff');
  assert.equal(message.blocks?.[1].type === 'artifact' ? message.blocks[1].artifact.status : undefined, 'complete');
}

function testArtifactContentRepeatedChunkIsPreservedAsDelta() {
  const content = [
    'typescript',
    'public async openInSidebar() {',
  ].join('\n');
  let state = startAssistantState();
  state = chatReducer(state, { type: 'ARTIFACT_START', id: 'artifact-snapshot', artifactType: 'code', language: 'text', title: 'text' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-snapshot', content });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-snapshot', content });

  const message = state.messages[0];
  assert.equal(message.artifacts?.[0]?.content, content + content);
  assert.equal(message.blocks?.[0].type === 'artifact' ? message.blocks[0].artifact.content : undefined, content + content);
}

function testArtifactContentPrefixChunkIsPreservedAsDelta() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'ARTIFACT_START', id: 'artifact-prefix', artifactType: 'code', language: 'text', title: 'text' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-prefix', content: 'typescript\npublic async' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-prefix', content: 'typescript\npublic async openInSidebar() {' });

  const message = state.messages[0];
  assert.equal(message.artifacts?.[0]?.content, 'typescript\npublic asynctypescript\npublic async openInSidebar() {');
  assert.equal(message.blocks?.[0].type === 'artifact' ? message.blocks[0].artifact.content : undefined, 'typescript\npublic asynctypescript\npublic async openInSidebar() {');
}

function testArtifactContentChunkStartingWithExistingTextIsStillAppended() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'ARTIFACT_START', id: 'artifact-delta', artifactType: 'code', language: 'text', title: 'text' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-delta', content: 'go' });
  state = chatReducer(state, { type: 'ARTIFACT_CONTENT', id: 'artifact-delta', content: 'go test ./...' });

  const message = state.messages[0];
  assert.equal(message.artifacts?.[0]?.content, 'gogo test ./...');
  assert.equal(message.blocks?.[0].type === 'artifact' ? message.blocks[0].artifact.content : undefined, 'gogo test ./...');
}

function testCodeArtifactLanguageSentinelIsRemovedBeforeRendering() {
  const normalized = normalizeCodeArtifactContent([
    'typescript',
    'public async openInSidebar() {',
  ].join('\n'), 'text');

  assert.equal(normalized.language, 'typescript');
  assert.equal(normalized.content, 'public async openInSidebar() {');
  const html = renderCodeBlockHtml(normalized.content, normalized.language);
  assert.doesNotMatch(html, /<code[^>]*>typescript/);
  assert.match(html, /openInSidebar/);
}

function testTypedCodeArtifactDoesNotStripDifferentLanguageLookingCodeLine() {
  const normalized = normalizeCodeArtifactContent([
    'go',
    'if [ -f go.mod ]; then',
    '  go test ./...',
    'fi',
  ].join('\n'), 'bash');

  assert.equal(normalized.language, 'bash');
  assert.equal(normalized.content, [
    'go',
    'if [ -f go.mod ]; then',
    '  go test ./...',
    'fi',
  ].join('\n'));
}

function testPlainCodeFenceArtifactDoesNotRenderArtifactChrome() {
  assert.equal(shouldRenderArtifactChrome({
    id: 'artifact-inline-code',
    artifactType: 'code',
    language: 'text',
    title: 'text',
    content: 'typescript\npublic async openInSidebar() {',
    status: 'streaming',
  }), false);

  assert.equal(shouldRenderArtifactChrome({
    id: 'artifact-named-file',
    artifactType: 'code',
    language: 'ts',
    title: 'src/provider.ts',
    content: 'public async openInSidebar() {',
    status: 'streaming',
  }), true);

  assert.equal(shouldRenderArtifactChrome({
    id: 'artifact-dockerfile',
    artifactType: 'code',
    language: 'dockerfile',
    title: 'Dockerfile',
    content: 'FROM node:22',
    status: 'streaming',
  }), true);

  assert.equal(shouldRenderArtifactChrome({
    id: 'artifact-readme',
    artifactType: 'markdown',
    language: 'md',
    title: 'README',
    content: '# Project',
    status: 'streaming',
  }), true);
}

function testToolBlocksStayBetweenTextChunks() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'before tool\n' });
  state = chatReducer(state, { type: 'TOOL_START', id: 'tool-1', name: 'read', args: '{"path":"file.ts"}' });
  state = chatReducer(state, { type: 'APPEND_TEXT', content: '\nafter tool' });
  state = chatReducer(state, { type: 'TOOL_RESULT', id: 'tool-1', name: 'read', output: 'ok', success: true, durationMs: 12 });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['text', 'tool', 'text']);
  assert.equal(message.blocks?.[1].type === 'tool' ? message.blocks[1].tool.status : undefined, 'done');
  assert.equal(message.blocks?.[1].type === 'tool' ? message.blocks[1].tool.output : undefined, 'ok');
}

function testHistoryAttachedSelectionMessageDisplaysOnlyUserQuestion() {
  const rawMessage = [
    'The user has attached the following file(s)/selection(s) for context. The content is provided inline below — DO NOT use read_file to re-read them.',
    '',
    'File: provider.ts (lines 27-38)',
    '```typescript',
    'interface SessionRuntime {',
    '  eventBuffer: Array<{',
    "    type: 'userMessage' | 'text';",
    '  }>',
    '}',
    '```',
    '',
    'User question: 分析这段代码',
  ].join('\n');

  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [{ role: 'user', content: rawMessage }],
  });

  assert.equal(state.messages[0].text, '分析这段代码');
  assert.equal(state.messages[0].contextFiles?.[0]?.fileName, 'provider.ts');
  assert.equal(state.messages[0].contextFiles?.[0]?.type, 'selection');
  assert.equal(state.messages[0].contextFiles?.[0]?.startLine, 27);
  assert.equal(state.messages[0].contextFiles?.[0]?.endLine, 38);
}

function testHistoryMissingImagePlaceholderIsPreserved() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [{
      role: 'user',
      content: '识别图片内容',
      images: [{ media_type: 'image/png', data: '', missing: true }],
    }],
  });

  assert.equal(state.messages[0].text, '识别图片内容');
  assert.equal(state.messages[0].images?.[0]?.missing, true);
}

function testHistoryRawVisionPreprocessTextDisplaysOriginalUserInput() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [{
      role: 'user',
      content: [
        '识别图片内容',
        '',
        '[图片内容（由 AtomGit-Qwen-Qwen3-VL-8B-Instruct 识别）]',
        '这是一张应用程序图标。',
      ].join('\n'),
    }],
  });

  assert.equal(state.messages[0].text, '识别图片内容');
  assert.equal(state.messages[0].images?.[0]?.missing, true);
}

function testTextArtifactWithMarkdownContentIsNotRenderedAsCodeArtifact() {
  const kind = classifyArtifactRenderKind({
    id: 'artifact-markdown',
    artifactType: 'text',
    language: 'text',
    content: [
      '输入:',
      '    diff --git a/foo b/foo',
      '',
      '旧版解析:',
      '    (ctx) diff --git a/foo b/foo',
      '',
      '新版解析:',
      '    (meta) diff --git a/foo b/foo',
    ].join('\n'),
    status: 'complete',
  });

  assert.equal(kind, 'markdown');
}

function testTextArtifactWithDiffContentIsStillRenderedAsDiff() {
  const kind = classifyArtifactRenderKind({
    id: 'artifact-diff',
    artifactType: 'text',
    language: 'text',
    content: [
      'diff',
      '- const oldValue = 1;',
      '+ const newValue = 1;',
    ].join('\n'),
    status: 'streaming',
  });

  assert.equal(kind, 'diff');
}

function testMarkdownArtifactLanguageSentinelBecomesFencedCodeBlock() {
  const normalized = normalizeMarkdownArtifactContent([
    'typescript',
    'eventBuffer: Array<{',
    "  type: 'userMessage' | 'text';",
    '  data: any;',
    '}>;',
    'typescript',
    'onArtifactStart: (id, artifactType) => {',
    '  return id;',
    '},',
  ].join('\n'));

  assert.match(normalized, /^```typescript\n/);
  assert.match(normalized, /\n```\n```typescript\n/);
  assert.doesNotMatch(normalized, /^typescript\n/);
}

function testMarkdownArtifactLanguageSentinelDoesNotSwallowFollowingProse() {
  const normalized = normalizeMarkdownArtifactContent([
    '修改点说明：',
    'typescript',
    'export interface ArtifactData {',
    '  id: string;',
    '}',
    '',
    '后续说明：',
    '这里应该继续作为 Markdown 正文。',
  ].join('\n'));

  assert.match(normalized, /修改点说明：\n```typescript\nexport interface ArtifactData \{/);
  assert.match(normalized, /\n```\n\n后续说明：/);
  assert.doesNotMatch(normalized, /```typescript[\s\S]*后续说明：[\s\S]*```/);
}

function testDiffLikeTypedCodeIsRenderedAsDiffRows() {
  const html = renderCodeBlockHtml(
    '+ export function chooseDesktopWorkspace(): Promise<WorkspaceActivationResult | null>',
    'ts',
  );

  assert.match(html, /class="[^"]*\bis-diff-like\b/);
  assert.match(html, /<span class="diff-code-gutter">\+<\/span>/);
  assert.match(html, /<span class="hljs-keyword">export<\/span>/);
  assert.doesNotMatch(html, /\+ <span class="hljs-keyword">export<\/span>/);
}

function testTextArtifactWithDiffSentinelIsRenderedAsDiffRows() {
  const html = renderCodeBlockHtml([
    'diff',
    '+ export interface ArtifactData {',
    '+   id: string;',
    '+ }',
  ].join('\n'), 'text');

  assert.match(html, /class="[^"]*\bis-diff-like\b/);
  assert.match(html, /<span class="diff-code-line diff-code-add">/);
  assert.match(html, /<span class="diff-code-gutter">\+<\/span>/);
  assert.doesNotMatch(html, /<span class="diff-code-content">diff<\/span>/);
}

function testRepeatedDiffSentinelsDuringStreamingStayDiffRendered() {
  const html = renderCodeBlockHtml([
    'diff',
    '- const oldValue = 1;',
    '+ const newValue = 1;',
    'diff',
    '- const oldName = 2;',
    '+ const newName = 2;',
  ].join('\n'), 'text');

  assert.match(html, /class="[^"]*\bis-diff-like\b/);
  assert.equal((html.match(/diff-code-del/g) ?? []).length, 2);
  assert.equal((html.match(/diff-code-add/g) ?? []).length, 2);
  assert.doesNotMatch(html, /<span class="diff-code-content">diff<\/span>/);
}

function testDiffBlankLinesAreClassedForCompactSpacing() {
  const html = renderCodeBlockHtml([
    '+ .code-block-wrapper {',
    '',
    '+   margin: 8px 0;',
  ].join('\n'), 'diff');

  assert.match(html, /class="diff-code-line diff-code-empty diff-code-ctx"/);
}

function testDiffRowsDoNotInsertPreformattedTextNodeSeparators() {
  const html = renderCodeBlockHtml([
    '+ first line',
    '+ second line',
  ].join('\n'), 'diff');

  assert.doesNotMatch(html, /<\/span>\n<span class="diff-code-line/);
  assert.match(html, /<\/span><span class="diff-code-line/);
}

function testArtifactStartDoesNotClearEarlierContent() {
  const base = {
    ...initialState,
    messages: [{
      id: 'assistant-1',
      role: 'assistant' as const,
      text: '',
      streaming: true,
      timestamp: 1,
    }],
  };

  const withContent = chatReducer(base, {
    type: 'ARTIFACT_CONTENT',
    id: 'artifact-1',
    content: '+ export function chooseDesktopWorkspace()',
  });
  const withMetadata = chatReducer(withContent, {
    type: 'ARTIFACT_START',
    id: 'artifact-1',
    artifactType: 'code',
    language: 'ts',
    title: 'src/api/desktopWorkspaceRuntime.ts',
  });

  const artifact = withMetadata.messages[0].artifacts?.[0];
  assert.equal(artifact?.content, '+ export function chooseDesktopWorkspace()');
  assert.equal(artifact?.language, 'ts');
  assert.equal(artifact?.title, 'src/api/desktopWorkspaceRuntime.ts');
}

function testUnifiedDiffMetadataIsNotTreatedAsChangedCode() {
  const lines = parseDiff([
    'diff --git a/src/file.ts b/src/file.ts',
    'index 1111111..2222222 100644',
    '--- a/src/file.ts',
    '+++ b/src/file.ts',
    '@@ -1,1 +1,1 @@',
    '-oldValue',
    '+newValue',
  ].join('\n'));

  assert.equal(lines[0].type, 'meta');
  assert.equal(lines[2].type, 'meta');
  assert.equal(lines[3].type, 'meta');
  assert.equal(lines[4].type, 'hunk');
  assert.equal(lines[5].type, 'del');
  assert.equal(lines[6].type, 'add');
}

function testDiffViewDropsDiffLanguageSentinel() {
  const lines = parseDiff([
    'diff',
    '@@ -1,1 +1,1 @@',
    '-oldValue',
    '+newValue',
  ].join('\n'));

  assert.equal(lines[0].type, 'hunk');
  assert.equal(lines[0].text, '@@ -1,1 +1,1 @@');
  assert.equal(lines.length, 3);
}

function testDiffSingleLineCssLetsBackgroundFillTheBlock() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.match(css, /\.code-block-wrapper\.is-diff-like\.is-single-line pre\s*\{[^}]*min-height:\s*0;/s);
  assert.match(css, /\.diff-code-line\s*\{[^}]*padding:\s*3px 36px 3px 0;/s);
  assert.match(css, /\.diff-code-gutter\s*\{[^}]*display:\s*flex;/s);
  assert.match(css, /\.diff-code-content\s*\{[^}]*display:\s*block;/s);
  assert.match(css, /\.diff-code-empty\s*\{[^}]*padding-top:\s*0;/s);
  assert.match(css, /\.diff-code-empty\s*\{[^}]*padding-bottom:\s*0;/s);
}

function testUserMessageContainerDoesNotForceMarkdownPreWrap() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.match(css, /\.user-message-bubble\s*\{[^}]*white-space:\s*normal;/s);
  assert.match(css, /\.user-message-text\s*\{[^}]*white-space:\s*normal;/s);
}

function testInlineArtifactCodeKeepsCodeBlockBorder() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.doesNotMatch(css, /(^|\n)\.artifact-code-render \.code-block-wrapper pre\s*\{[^}]*border:\s*none;/s);
  assert.match(css, /\.artifact-block \.artifact-code-render \.code-block-wrapper pre\s*\{[^}]*border:\s*none;/s);
}

function testPreCodeDoesNotUseInlineCodePillStyling() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.match(css, /\.markdown-root pre code\s*\{[^}]*background:\s*transparent;/s);
  assert.match(css, /\.markdown-root pre code\s*\{[^}]*border-radius:\s*0;/s);
  assert.match(css, /\.markdown-root pre code\s*\{[^}]*padding:\s*0;/s);
}

function testMissingUserImagePlaceholderHasStableThumbnailSizing() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.match(css, /\.user-message-image-placeholder\s*\{[^}]*width:\s*min\(180px,\s*100%\);/s);
  assert.match(css, /\.user-message-image-placeholder\s*\{[^}]*height:\s*120px;/s);
}

function testStreamingMarkdownRepairsUnclosedCodeFence() {
  const repaired = repairStreamingMarkdown([
    '说明：',
    '```rust',
    'fn main() {',
    '  println!("hi");',
  ].join('\n'));

  assert.equal(repaired, [
    '说明：',
    '```rust',
    'fn main() {',
    '  println!("hi");',
    '```',
    '',
  ].join('\n'));
}

function testStreamingMarkdownLeavesClosedCodeFenceUnchanged() {
  const markdown = [
    '说明：',
    '```rust',
    'fn main() {}',
    '```',
    '后续正文',
  ].join('\n');

  assert.equal(repairStreamingMarkdown(markdown), markdown);
}

function testFinalMarkdownProtectsFenceInsideInlineCodeSpan() {
  const markdown = [
    '原本只检查 `text.starts_with("',
    '```',
    '")` 和 `text.trim() == "```"`，限制很大。',
  ].join('\n');
  const prepared = prepareMarkdownForRender(markdown, false);
  const html = renderMarkdownForTest(prepared);

  assert.doesNotMatch(html, /<pre><code>/);
  assert.match(html, /<code>text\.starts_with/);
  assert.match(html, /```/);
}

function testGenerationDoneReloadsFinishedSessionHistory() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  const onDone = source.match(/onDone:\s*\([^)]*\)\s*=>\s*\{[\s\S]*?\n\s*\},\n\s*onStopped:/)?.[0] ?? '';

  assert.match(onDone, /this\._reloadFinishedSessionHistory\(sessionId \|\| streamSessionId\)/);
}

testDiffLikeTypedCodeIsRenderedAsDiffRows();
testTextArtifactWithDiffSentinelIsRenderedAsDiffRows();
testRepeatedDiffSentinelsDuringStreamingStayDiffRendered();
testDiffBlankLinesAreClassedForCompactSpacing();
testDiffRowsDoNotInsertPreformattedTextNodeSeparators();
testStreamingBlocksPreserveTextArtifactTextOrder();
testArtifactContentBeforeStartKeepsBlockPosition();
testArtifactContentRepeatedChunkIsPreservedAsDelta();
testArtifactContentPrefixChunkIsPreservedAsDelta();
testArtifactContentChunkStartingWithExistingTextIsStillAppended();
testCodeArtifactLanguageSentinelIsRemovedBeforeRendering();
testTypedCodeArtifactDoesNotStripDifferentLanguageLookingCodeLine();
testPlainCodeFenceArtifactDoesNotRenderArtifactChrome();
testToolBlocksStayBetweenTextChunks();
testHistoryAttachedSelectionMessageDisplaysOnlyUserQuestion();
testHistoryMissingImagePlaceholderIsPreserved();
testHistoryRawVisionPreprocessTextDisplaysOriginalUserInput();
testTextArtifactWithMarkdownContentIsNotRenderedAsCodeArtifact();
testTextArtifactWithDiffContentIsStillRenderedAsDiff();
testMarkdownArtifactLanguageSentinelBecomesFencedCodeBlock();
testMarkdownArtifactLanguageSentinelDoesNotSwallowFollowingProse();
testArtifactStartDoesNotClearEarlierContent();
testUnifiedDiffMetadataIsNotTreatedAsChangedCode();
testDiffViewDropsDiffLanguageSentinel();
testDiffSingleLineCssLetsBackgroundFillTheBlock();
testUserMessageContainerDoesNotForceMarkdownPreWrap();
testInlineArtifactCodeKeepsCodeBlockBorder();
testPreCodeDoesNotUseInlineCodePillStyling();
testMissingUserImagePlaceholderHasStableThumbnailSizing();
testStreamingMarkdownRepairsUnclosedCodeFence();
testStreamingMarkdownLeavesClosedCodeFenceUnchanged();
testFinalMarkdownProtectsFenceInsideInlineCodeSpan();
testGenerationDoneReloadsFinishedSessionHistory();
