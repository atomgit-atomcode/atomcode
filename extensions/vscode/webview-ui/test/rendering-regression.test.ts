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
import { markdownToHtml } from '../src/components/Markdown';
import { prepareMarkdownForRender, repairStreamingMarkdown } from '../src/components/streamingMarkdown';
import { formatToolDuration } from '../src/utils/format';
import { shouldShowIdleNotice } from '../src/utils/streamStatus';

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

function testLogoutRequiresSetupOnlyForLoginDependentProvider() {
  const provider = (name: string, requiresLogin: boolean) => ({
    name,
    type: 'openai',
    model: 'model',
    has_api_key: false,
    requires_login: requiresLogin,
    is_default: true,
    context_window: 128_000,
    skip_tls_verify: false,
  });
  const signedOut = {
    logged_in: false,
    expired: false,
    auth_path: '/tmp/auth.toml',
    user: null,
  };

  let state = chatReducer(initialState, {
    type: 'SET_PROVIDERS',
    providers: [provider('custom', false)],
    defaultProvider: 'custom',
  });
  state = chatReducer(state, { type: 'SET_AUTH', auth: signedOut });
  assert.equal(state.setupRequired, false);

  state = chatReducer(state, {
    type: 'SET_PROVIDERS',
    providers: [provider('gateway', true)],
    defaultProvider: 'gateway',
  });
  assert.equal(state.setupRequired, true);
}

function testToolDurationFormattingUsesMillisecondsBelowOneSecond() {
  assert.equal(formatToolDuration(0), '1ms');
  assert.equal(formatToolDuration(1), '1ms');
  assert.equal(formatToolDuration(99), '99ms');
  assert.equal(formatToolDuration(999), '999ms');
  assert.equal(formatToolDuration(1000), '1.0s');
  assert.equal(formatToolDuration(1234), '1.2s');
}

function testWarningAddsStatusBlockToStreamingAssistantMessage() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'hello\n' });
  state = chatReducer(state, { type: 'STREAM_WARNING', message: 'network is slow' });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['text', 'status']);
  assert.equal(message.blocks?.[1].type === 'status' ? message.blocks[1].status.kind : undefined, 'warning');
  assert.equal(message.blocks?.[1].type === 'status' ? message.blocks[1].status.message : undefined, 'network is slow');
}

function testRateLimitedStatusBlockIsUpdatedInPlace() {
  let state = startAssistantState();
  state = chatReducer(state, {
    type: 'STREAM_RATE_LIMITED',
    message: 'rate limited',
    retryAfterSeconds: 3,
    attempt: 1,
    maxAttempts: 5,
  });
  state = chatReducer(state, {
    type: 'STREAM_RATE_LIMITED',
    message: 'rate limited again',
    retryAfterSeconds: 1,
    attempt: 2,
    maxAttempts: 5,
  });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['status']);
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.kind : undefined, 'rate_limited');
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.message : undefined, 'rate limited again');
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.retryAfterSeconds : undefined, 1);
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.attempt : undefined, 2);
}

function testDoneMarksRunningToolsIncompleteWithoutResult() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'TOOL_START', id: 'tool-1', name: 'read', args: '{"path":"file.ts"}' });
  state = chatReducer(state, { type: 'GENERATION_DONE', usage: {} });

  const tool = state.messages[0].toolCalls?.[0];
  assert.equal(tool?.status, 'incomplete');
  assert.equal(state.messages[0].blocks?.[0].type === 'tool' ? state.messages[0].blocks[0].tool.status : undefined, 'incomplete');
}

function testResumeStreamingReplayIsIdempotent() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'already streamed' });
  state = chatReducer(state, { type: 'RESUME_STREAMING' });

  assert.equal(
    state.messages.filter((message) => message.role === 'assistant' && message.streaming).length,
    1,
    'replaying resumeStreaming must not append a second live assistant bubble',
  );
}

function testToolBatchReplayUpsertsCallsById() {
  let state = startAssistantState();
  const action = {
    type: 'TOOL_BATCH_START' as const,
    calls: [{ id: 'tool-1', name: 'read_file', args: '{"path":"a.ts"}' }],
  };
  state = chatReducer(state, action);
  state = chatReducer(state, action);

  assert.equal(state.messages[0].toolCalls?.length, 1);
  assert.equal(
    state.messages[0].blocks?.filter((block) => block.type === 'tool').length,
    1,
  );
}

function testErrorMarksRunningToolsError() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'TOOL_START', id: 'tool-1', name: 'read', args: '{"path":"file.ts"}' });
  state = chatReducer(state, { type: 'GENERATION_ERROR', message: 'stream closed' });

  const tool = state.messages[0].toolCalls?.[0];
  assert.equal(tool?.status, 'error');
  assert.equal(tool?.output, 'stream closed');
  assert.equal(state.messages[0].blocks?.[0].type === 'tool' ? state.messages[0].blocks[0].tool.status : undefined, 'error');
}

function testToolProgressReplacesLatestActivity() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'TOOL_START', id: 'review-1', name: 'code_review', args: '{}' });
  state = chatReducer(state, { type: 'TOOL_PROGRESS', id: 'review-1', progress: 'round 1 · thinking' });
  state = chatReducer(state, { type: 'TOOL_PROGRESS', id: 'review-1', progress: 'round 2 · read_file' });

  assert.equal(state.messages[0].toolCalls?.[0]?.progress, 'round 2 · read_file');
}

function testPartialReviewResultIsMarkedIncomplete() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'TOOL_START', id: 'review-1', name: 'code_review', args: '{}' });
  state = chatReducer(state, {
    type: 'TOOL_RESULT',
    id: 'review-1',
    name: 'code_review',
    output: 'Code review incomplete (MaxRounds)',
    success: false,
    durationMs: 600_000,
  });

  assert.equal(state.messages[0].toolCalls?.[0]?.status, 'incomplete');
}

function testIdleNoticeAddsSingleStatusBlock() {
  let state = startAssistantState();
  state = chatReducer(state, { type: 'STREAM_IDLE_NOTICE', message: 'still waiting' });
  state = chatReducer(state, { type: 'STREAM_IDLE_NOTICE', message: 'still waiting again' });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['status']);
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.kind : undefined, 'idle');
  assert.equal(message.blocks?.[0].type === 'status' ? message.blocks[0].status.message : undefined, 'still waiting again');
}

function testIdleNoticePredicateRequiresGeneratingAndThreshold() {
  assert.equal(shouldShowIdleNotice({
    isGenerating: false,
    lastEventAt: 1_000,
    now: 16_000,
    thresholdMs: 15_000,
    alreadyShown: false,
  }), false);
  assert.equal(shouldShowIdleNotice({
    isGenerating: true,
    lastEventAt: 1_000,
    now: 15_999,
    thresholdMs: 15_000,
    alreadyShown: false,
  }), false);
  assert.equal(shouldShowIdleNotice({
    isGenerating: true,
    lastEventAt: 1_000,
    now: 16_000,
    thresholdMs: 15_000,
    alreadyShown: true,
  }), false);
  assert.equal(shouldShowIdleNotice({
    isGenerating: true,
    lastEventAt: 1_000,
    now: 16_000,
    thresholdMs: 15_000,
    alreadyShown: false,
  }), true);
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

function testPermissionRequestMarksMatchingToolWaitingAndAddsPermissionBlock() {
  let state = startAssistantState();
  state = chatReducer(state, {
    type: 'TOOL_START',
    id: 'call-1',
    name: 'write_file',
    args: '{"path":"README.md"}',
  });
  state = chatReducer(state, {
    type: 'PERMISSION_REQUEST',
    id: 'call-1',
    sessionId: 'session-1',
    toolName: 'write_file',
    reason: 'Modify workspace file',
    args: '{"path":"README.md"}',
    isDestructive: true,
  });

  const message = state.messages[0];
  assert.deepEqual(message.blocks?.map((block) => block.type), ['tool', 'permission']);
  assert.equal(message.toolCalls?.[0]?.status, 'waiting_approval');
  assert.equal(message.permissionRequest?.id, 'call-1');
  assert.equal(message.permissionRequest?.sessionId, 'session-1');
  assert.equal(message.permissionRequest?.reason, 'Modify workspace file');
}

function testConsecutivePermissionResponsesUpdateOriginalBlockOnly() {
  let state = startAssistantState();
  state = chatReducer(state, {
    type: 'TOOL_START',
    id: 'call-1',
    name: 'write_file',
    args: '{"path":"README.md"}',
  });
  state = chatReducer(state, {
    type: 'PERMISSION_REQUEST',
    id: 'call-1',
    sessionId: 'session-1',
    toolName: 'write_file',
    reason: 'Modify first file',
    args: '{"path":"README.md"}',
    isDestructive: true,
  });
  state = chatReducer(state, { type: 'PERMISSION_RESPOND', id: 'call-1', decision: 'allow' });
  state = chatReducer(state, {
    type: 'TOOL_START',
    id: 'call-2',
    name: 'write_file',
    args: '{"path":"CHANGELOG.md"}',
  });
  state = chatReducer(state, {
    type: 'PERMISSION_REQUEST',
    id: 'call-2',
    sessionId: 'session-1',
    toolName: 'write_file',
    reason: 'Modify second file',
    args: '{"path":"CHANGELOG.md"}',
    isDestructive: true,
  });
  state = chatReducer(state, { type: 'PERMISSION_RESPONSE_RESULT', id: 'call-1', success: true });

  const message = state.messages[0];
  const permissionBlocks = message.blocks?.filter((block) => block.type === 'permission') ?? [];
  assert.deepEqual(
    permissionBlocks.map((block) => block.type === 'permission' ? [block.request.id, block.request.status] : undefined),
    [['call-1', 'allowed'], ['call-2', 'pending']],
  );
  assert.equal(message.permissionRequest?.id, 'call-2');
  assert.equal(message.permissionRequest?.status, 'pending');
}

function testPermissionRespondStoresExplicitDecision() {
  let state = startAssistantState();
  state = chatReducer(state, {
    type: 'PERMISSION_REQUEST',
    id: 'call-1',
    sessionId: 'session-1',
    toolName: 'mcp__server__tool',
    reason: 'Run MCP tool',
    args: '{}',
    isDestructive: false,
  });
  state = chatReducer(state, { type: 'PERMISSION_RESPOND', id: 'call-1', decision: 'allow_persist' });

  const request = state.messages[0].permissionRequest;
  assert.equal(request?.status, 'submitting');
  assert.equal(request?.decision, 'allow_persist');
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

function testHistorySyntheticUserMessagesAreHidden() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'real prompt' },
      { role: 'user', content: 'You made code edits but have not verified them.', synthetic: true },
      { role: 'assistant', content: 'reply' },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => [msg.role, msg.text]), [
    ['user', 'real prompt'],
    ['assistant', 'reply'],
  ]);
}

function testHistoryVerifyCadenceAssistantMessagesAreHidden() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'create f.txt' },
      { role: 'assistant', content: '', tool_calls: [{ id: 'w1', name: 'write_file', arguments: '{}' }] },
      { role: 'tool', content: 'ok', tool_result: { call_id: 'w1', success: true, summary: 'ok', line_count: 1 } },
      { role: 'user', content: 'You made code edits but have not verified them.', synthetic: true },
      { role: 'assistant', content: 'No verification is needed.', internal_origin: 'verify_cadence' },
      { role: 'user', content: 'what model are you' },
      { role: 'assistant', content: 'I am AtomCode.' },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => [msg.role, msg.text]), [
    ['user', 'create f.txt'],
    ['assistant', ''],
    ['user', 'what model are you'],
    ['assistant', 'I am AtomCode.'],
  ]);
}

function testHistoryVerifyCadenceCamelCaseAssistantMessagesAreHidden() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'create f.txt' },
      { role: 'assistant', content: 'No verification is needed.', internalOrigin: 'verify_cadence' },
      { role: 'assistant', content: 'I am AtomCode.' },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => [msg.role, msg.text]), [
    ['user', 'create f.txt'],
    ['assistant', 'I am AtomCode.'],
  ]);
}

function testHistoryVerifyCadenceAssistantWithToolCallsIsVisible() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      {
        role: 'assistant',
        content: 'Running verification',
        internal_origin: 'verify_cadence',
        tool_calls: [{ id: 'b1', name: 'bash', arguments: '{"command":"true"}' }],
      },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => [msg.role, msg.text, msg.toolCalls?.length ?? 0]), [
    ['assistant', 'Running verification', 1],
  ]);
}

function testHistoryLegacyInternalUserMessagesAreHidden() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'real prompt' },
      { role: 'user', content: 'You made code edits but have not verified them. Run a fast check (`cargo check`).' },
      { role: 'user', content: '<system-reminder>\nCurrent task list\n</system-reminder>' },
      { role: 'user', content: '[Auto-read from error: src/main.rs]\nfn main() {}' },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => msg.text), ['real prompt']);
}

function testHistoryUserMessageStartingWithLegacyWordsIsVisible() {
  const state = chatReducer({
    ...initialState,
    messages: [],
    queuedMessages: [],
  }, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'Output limit hit when running pytest; how do I debug it?' },
    ],
  });

  assert.deepEqual(state.messages.map((msg) => msg.text), [
    'Output limit hit when running pytest; how do I debug it?',
  ]);
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

function testUserMessageRendersPlainTextInsteadOfMarkdown() {
  const source = readFileSync(join(process.cwd(), 'webview-ui/src/components/UserMessage.tsx'), 'utf8');

  assert.doesNotMatch(source, /import\s+\{\s*Markdown\s*\}/);
  assert.doesNotMatch(source, /<Markdown\s+content=\{message\.text\}/);
  assert.match(source, /className="user-message-plain-text"/);
  // When no search query is active, the raw message text is rendered as-is
  // (search highlights are injected only when a query is present).
  assert.match(source, /: message\.text\}/);
}

function testUserPlainTextCssPreservesLiteralInput() {
  const css = readFileSync(join(process.cwd(), 'webview-ui/src/styles/messages.css'), 'utf8');

  assert.match(css, /\.user-message-plain-text\s*\{[^}]*white-space:\s*pre-wrap;/s);
  assert.match(css, /\.user-message-plain-text\s*\{[^}]*overflow-wrap:\s*anywhere;/s);
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

function testPermissionCardStaysVisibleWhileDecisionIsSubmitting() {
  const source = readFileSync(join(process.cwd(), 'webview-ui/src/components/AssistantMessage.tsx'), 'utf8');

  assert.match(source, /block\.request\.status === 'pending' \|\| block\.request\.status === 'submitting'/);
}

function testPermissionCardOffersPersistentDecisionOptions() {
  const source = readFileSync(join(process.cwd(), 'webview-ui/src/components/PermissionRequest.tsx'), 'utf8');

  assert.match(source, /handleRespond\('always_allow'\)/);
  assert.match(source, /handleRespond\('allow_persist'\)/);
  assert.match(source, /request\.toolName\.startsWith\('mcp__'\)/);
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

function testMarkdownRawHtmlIsEscapedInsteadOfDroppedBySanitizer() {
  const html = markdownToHtml('请输出一段 </script> 和 <script>alert(1)</script> 文本');

  assert.match(html, /&lt;\/script&gt;/);
  assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
  assert.doesNotMatch(html, /<script>/);
}

function testMarkdownTableDoesNotSwallowFollowingPlainText() {
  const markdown = [
    '| 示例（Pandoc） | 示例（Slidev） |',
    '|--|--|',
    '| image | image |',
    '后续内容',
  ].join('\n');
  const html = renderMarkdownForTest(prepareMarkdownForRender(markdown, false));

  assert.match(html, /<\/table>\s*<p>后续内容<\/p>/);
  assert.doesNotMatch(html, /<td>后续内容<\/td>/);
}

function testMarkdownTableWithSingleDashDelimiterDoesNotSwallowFollowingPlainText() {
  const markdown = [
    '| A | B |',
    '|-|-|',
    '| x | y |',
    '后续内容',
  ].join('\n');
  const html = renderMarkdownForTest(prepareMarkdownForRender(markdown, false));

  assert.match(html, /<\/table>\s*<p>后续内容<\/p>/);
  assert.doesNotMatch(html, /<td>后续内容<\/td>/);
}

function testMarkdownTableDoesNotSwallowFollowingFencedCode() {
  const markdown = [
    '| A | B |',
    '|--|--|',
    '| x | y |',
    '```ts',
    'const value = 1;',
    '```',
  ].join('\n');
  const html = renderMarkdownForTest(prepareMarkdownForRender(markdown, false));

  assert.match(html, /<\/table>\s*<pre><code class="language-ts">const value = 1;/);
  assert.doesNotMatch(html, /<td>```ts<\/td>/);
}

function testMarkdownTableRepairDoesNotChangeFencedCodeSamples() {
  const markdown = [
    '```markdown',
    '| A | B |',
    '|--|--|',
    '| x | y |',
    'plain',
    '```',
  ].join('\n');

  assert.equal(prepareMarkdownForRender(markdown, false), markdown);
}

function testMarkdownTableRepairDoesNotChangeHtmlBlocks() {
  const markdown = [
    '<div>',
    '| A | B |',
    '|--|--|',
    'plain',
    '</div>',
  ].join('\n');

  assert.equal(prepareMarkdownForRender(markdown, false), markdown);
}

function testMarkdownTableRepairKeepsMarkedOneColumnRows() {
  const markdown = [
    '| A |',
    '|--|',
    'plain',
  ].join('\n');
  const html = renderMarkdownForTest(prepareMarkdownForRender(markdown, false));

  assert.match(html, /<td>plain<\/td>/);
  assert.doesNotMatch(html, /<p>plain<\/p>/);
}

function testGenerationDoneReloadsFinishedSessionHistory() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  const onDone = source.match(/onDone:\s*\([^)]*\)\s*=>\s*\{[\s\S]*?\n\s*\},\n\s*onStopped:/)?.[0] ?? '';

  assert.match(onDone, /const doneSessionId = sessionId \|\| streamSessionId/);
  assert.match(onDone, /this\._reloadFinishedSessionHistory\(doneSessionId, streamGeneration\)/);
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
testPermissionRequestMarksMatchingToolWaitingAndAddsPermissionBlock();
testConsecutivePermissionResponsesUpdateOriginalBlockOnly();
testPermissionRespondStoresExplicitDecision();
testHistoryAttachedSelectionMessageDisplaysOnlyUserQuestion();
testHistoryMissingImagePlaceholderIsPreserved();
testHistoryRawVisionPreprocessTextDisplaysOriginalUserInput();
testHistorySyntheticUserMessagesAreHidden();
testHistoryVerifyCadenceAssistantMessagesAreHidden();
testHistoryVerifyCadenceCamelCaseAssistantMessagesAreHidden();
testHistoryVerifyCadenceAssistantWithToolCallsIsVisible();
testHistoryLegacyInternalUserMessagesAreHidden();
testHistoryUserMessageStartingWithLegacyWordsIsVisible();
testTextArtifactWithMarkdownContentIsNotRenderedAsCodeArtifact();
testTextArtifactWithDiffContentIsStillRenderedAsDiff();
testMarkdownArtifactLanguageSentinelBecomesFencedCodeBlock();
testMarkdownArtifactLanguageSentinelDoesNotSwallowFollowingProse();
testArtifactStartDoesNotClearEarlierContent();
testUnifiedDiffMetadataIsNotTreatedAsChangedCode();
testDiffViewDropsDiffLanguageSentinel();
testDiffSingleLineCssLetsBackgroundFillTheBlock();
testUserMessageContainerDoesNotForceMarkdownPreWrap();
testUserMessageRendersPlainTextInsteadOfMarkdown();
testUserPlainTextCssPreservesLiteralInput();
testInlineArtifactCodeKeepsCodeBlockBorder();
testPreCodeDoesNotUseInlineCodePillStyling();
testMissingUserImagePlaceholderHasStableThumbnailSizing();
testPermissionCardStaysVisibleWhileDecisionIsSubmitting();
testPermissionCardOffersPersistentDecisionOptions();
testStreamingMarkdownRepairsUnclosedCodeFence();
testStreamingMarkdownLeavesClosedCodeFenceUnchanged();
testFinalMarkdownProtectsFenceInsideInlineCodeSpan();
testMarkdownRawHtmlIsEscapedInsteadOfDroppedBySanitizer();
testMarkdownTableDoesNotSwallowFollowingPlainText();
testMarkdownTableWithSingleDashDelimiterDoesNotSwallowFollowingPlainText();
testMarkdownTableDoesNotSwallowFollowingFencedCode();
testMarkdownTableRepairDoesNotChangeFencedCodeSamples();
testMarkdownTableRepairDoesNotChangeHtmlBlocks();
testMarkdownTableRepairKeepsMarkedOneColumnRows();
testGenerationDoneReloadsFinishedSessionHistory();
testLogoutRequiresSetupOnlyForLoginDependentProvider();
testToolDurationFormattingUsesMillisecondsBelowOneSecond();
testWarningAddsStatusBlockToStreamingAssistantMessage();
testRateLimitedStatusBlockIsUpdatedInPlace();
testDoneMarksRunningToolsIncompleteWithoutResult();
testResumeStreamingReplayIsIdempotent();
testToolBatchReplayUpsertsCallsById();
testErrorMarksRunningToolsError();
testToolProgressReplacesLatestActivity();
testPartialReviewResultIsMarkedIncomplete();
testIdleNoticeAddsSingleStatusBlock();
testIdleNoticePredicateRequiresGeneratingAndThreshold();
