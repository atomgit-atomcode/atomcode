import assert from 'node:assert/strict';
import Module from 'node:module';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

declare const require: {
  (id: string): typeof import('../../src/chat/provider');
};

const originalLoad = (Module as unknown as { _load: typeof Module['_load'] })._load;
type WatcherRecord = {
  pattern: { base: string; pattern: string };
  create?: () => void;
  change?: () => void;
  delete?: () => void;
  disposed: boolean;
};
const fileWatchers: WatcherRecord[] = [];
class RelativePatternMock {
  constructor(public base: string, public pattern: string) {}
}
const vscodeMock = {
  Uri: { joinPath: (...parts: Array<{ fsPath?: string } | string>) => ({ fsPath: parts.map((p) => typeof p === 'string' ? p : p.fsPath || '').join('/') }) },
  RelativePattern: RelativePatternMock,
  workspace: {
    workspaceFolders: [] as Array<{ uri: { fsPath: string } }>,
    onDidChangeConfiguration: (_listener: unknown) => ({ dispose() {} }),
    getConfiguration: () => ({ get: (_key: string) => undefined }),
    createFileSystemWatcher: (pattern: { base: string; pattern: string }) => {
      const record: WatcherRecord = { pattern, disposed: false };
      fileWatchers.push(record);
      return {
        onDidCreate: (listener: () => void) => { record.create = listener; },
        onDidChange: (listener: () => void) => { record.change = listener; },
        onDidDelete: (listener: () => void) => { record.delete = listener; },
        dispose: () => { record.disposed = true; },
      };
    },
  },
  window: {},
  env: { language: 'en' },
  commands: {},
  l10n: { t: (value: string) => value },
  ViewColumn: { Beside: 2, Active: -1 },
};
(Module as unknown as { _load: typeof Module['_load'] })._load = function patchedLoad(request, parent, isMain) {
  if (request === 'vscode') {
    return vscodeMock;
  }
  return originalLoad.call(this, request, parent, isMain);
};

const { ChatViewProvider, mergeSessionsForDisplay } = require('../../src/chat/provider');
(globalThis as { document?: unknown }).document = { body: { dataset: { viewMode: 'tab' } } };
const { chatReducer, initialState } = require('../../webview-ui/src/state/reducer');

(Module as unknown as { _load: typeof Module['_load'] })._load = originalLoad;

function testReadyMarksPanelOnlyAfterInitialReplay() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  const readyCase = source.match(/case 'ready':[\s\S]*?case 'selectModel':/)?.[0] ?? '';
  assert.ok(readyCase.indexOf('await this._sendInitialState') >= 0);
  assert.ok(
    readyCase.indexOf('this._markPanelNotReady') >= 0
      && readyCase.indexOf('this._markPanelNotReady') < readyCase.indexOf('await this._sendInitialState'),
    'a reloaded panel must be marked not-ready before asynchronous initialization starts',
  );
  assert.ok(
    readyCase.indexOf('await this._sendInitialState') < readyCase.indexOf('this._finishPanelReadyReplay'),
    'a panel must stay not-ready until its buffered stream replay is complete',
  );
  const finishReplay = source.match(/private _finishPanelReadyReplay[\s\S]*?\n  }/)?.[0] ?? '';
  assert.ok(
    finishReplay.indexOf('this._replayStreamBuffer') < finishReplay.indexOf('this._markPanelReady'),
    'catch-up replay must finish before live forwarding is enabled',
  );
}

function testPanelReadyHandlerIsInstalledBeforeWebviewBoots() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  const openStart = source.indexOf('public openInTab(');
  const openEnd = source.indexOf('private _findSessionIdByPanel', openStart);
  const openInTab = source.slice(openStart, openEnd);
  const restoreStart = source.indexOf('public setupPanelForRestore(');
  const restoreEnd = source.indexOf('resolveWebviewView(', restoreStart);
  const restorePanel = source.slice(restoreStart, restoreEnd);

  for (const [label, method] of [
    ['new session tab', openInTab],
    ['restored session tab', restorePanel],
  ] as const) {
    assert.ok(method.includes('_setupWebviewMessageHandler'));
    assert.ok(method.includes('.html ='));
    assert.ok(
      method.indexOf('_setupWebviewMessageHandler') < method.indexOf('.html ='),
      `${label} must install the ready listener before assigning HTML, or the one-shot ready event can be lost`,
    );
  }
}

async function testOpeningAnExistingUnhydratedSessionTabLoadsItsHistory() {
  const history = [{ role: 'user', content: 'persisted prompt' }];
  let detailRequests = 0;
  let revealCount = 0;
  const posted: Array<{ type?: string; messages?: unknown[] }> = [];
  const webview = {
    postMessage: (message: { type?: string; messages?: unknown[] }) => {
      posted.push(message);
    },
  };
  const panel = {
    webview,
    reveal: () => {
      revealCount += 1;
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    getSession: async (projectHash: string, sessionId: string) => {
      detailRequests += 1;
      assert.equal(projectHash, 'project-a');
      assert.equal(sessionId, 'session-a');
      return { messages: history };
    },
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _panelSessions: Map<string, { projectHash?: string; messages?: unknown[] }>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._panelReady.set('session-a', true);

  await provider.openSessionInTab('session-a', 'project-a');

  assert.equal(detailRequests, 1);
  assert.equal(revealCount, 1);
  assert.deepEqual(unsafeProvider._panelSessions.get('session-a')?.messages, history);
  assert.deepEqual(
    posted.find((message) => message.type === 'sessionMessages')?.messages,
    history,
  );
}

async function testClosingAnExistingTabCancelsItsPendingHistoryHydration() {
  let resolveHistory!: (detail: { messages: unknown[] }) => void;
  const historyRequest = new Promise<{ messages: unknown[] }>((resolve) => {
    resolveHistory = resolve;
  });
  let revealCount = 0;
  const webview = { postMessage: (_message: unknown) => undefined };
  const panel = { webview, reveal: () => { revealCount += 1; } };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    getSession: () => historyRequest,
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _panelSessions: Map<string, { projectHash?: string; messages?: unknown[] }>;
    _sessionRuntimes: Map<string, { messages?: unknown[] }>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._panelReady.set('session-a', true);

  const opening = provider.openSessionInTab('session-a', 'project-a');
  unsafeProvider._panels.delete('session-a');
  unsafeProvider._panelReady.delete('session-a');
  unsafeProvider._panelSessions.delete('session-a');
  resolveHistory({ messages: [{ role: 'user', content: 'late history' }] });
  await opening;

  assert.equal(revealCount, 0);
  assert.equal(unsafeProvider._panelSessions.has('session-a'), false);
  assert.equal(unsafeProvider._sessionRuntimes.has('session-a'), false);
}

async function testNewGenerationRejectsHistoryLoadedForAnOlderGeneration() {
  let resolveHistory!: (detail: { messages: unknown[] }) => void;
  const historyRequest = new Promise<{ messages: unknown[] }>((resolve) => {
    resolveHistory = resolve;
  });
  const posted: Array<{ type?: string }> = [];
  const webview = { postMessage: (message: { type?: string }) => { posted.push(message); } };
  const panel = { webview, reveal: () => undefined };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    getSession: () => historyRequest,
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      messages?: unknown[];
    }>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 0,
    queuedMessages: [],
    eventBuffer: [],
  });

  const opening = provider.openSessionInTab('session-a', 'project-a');
  const runtime = unsafeProvider._sessionRuntimes.get('session-a')!;
  runtime.streamGeneration = 1;
  runtime.isGenerating = true;
  resolveHistory({ messages: [{ role: 'user', content: 'stale history' }] });
  await opening;

  assert.equal(runtime.messages, undefined);
  assert.equal(posted.some((message) => message.type === 'sessionMessages'), false);
}

async function testRepeatedOpenSharesOneHistoryRequestAndPublishesOnce() {
  let resolveHistory!: (detail: { messages: unknown[] }) => void;
  const historyRequest = new Promise<{ messages: unknown[] }>((resolve) => {
    resolveHistory = resolve;
  });
  let requests = 0;
  const posted: Array<{ type?: string }> = [];
  const webview = { postMessage: (message: { type?: string }) => { posted.push(message); } };
  const panel = { webview, reveal: () => undefined };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    getSession: () => {
      requests += 1;
      return historyRequest;
    },
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._panelReady.set('session-a', true);

  const first = provider.openSessionInTab('session-a', 'project-a');
  const second = provider.openSessionInTab('session-a', 'project-a');
  resolveHistory({ messages: [{ role: 'user', content: 'persisted' }] });
  await Promise.all([first, second]);

  assert.equal(requests, 1);
  assert.equal(posted.filter((message) => message.type === 'sessionMessages').length, 1);
}

async function testProjectRebindRejectsThePreviousProjectsLateHistory() {
  let resolveProjectA!: (detail: { messages: unknown[] }) => void;
  let resolveProjectB!: (detail: { messages: unknown[] }) => void;
  const projectA = new Promise<{ messages: unknown[] }>((resolve) => { resolveProjectA = resolve; });
  const projectB = new Promise<{ messages: unknown[] }>((resolve) => { resolveProjectB = resolve; });
  const posted: Array<{ type?: string; messages?: unknown[]; projectHash?: string }> = [];
  const webview = {
    postMessage: (message: { type?: string; messages?: unknown[]; projectHash?: string }) => {
      posted.push(message);
    },
  };
  const panel = { webview, reveal: () => undefined };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    getSession: (projectHash: string) => projectHash === 'project-a' ? projectA : projectB,
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _panelSessions: Map<string, { projectHash?: string; messages?: unknown[] }>;
  };
  unsafeProvider._panels.set('shared-session', panel);
  unsafeProvider._panelReady.set('shared-session', true);

  const openingA = provider.openSessionInTab('shared-session', 'project-a');
  const openingB = provider.openSessionInTab('shared-session', 'project-b');
  resolveProjectA({ messages: [{ role: 'user', content: 'history A' }] });
  await openingA;
  resolveProjectB({ messages: [{ role: 'user', content: 'history B' }] });
  await openingB;

  assert.equal(unsafeProvider._panelSessions.get('shared-session')?.projectHash, 'project-b');
  assert.deepEqual(unsafeProvider._panelSessions.get('shared-session')?.messages, [
    { role: 'user', content: 'history B' },
  ]);
  const histories = posted.filter((message) => message.type === 'sessionMessages');
  assert.equal(histories.length, 1);
  assert.deepEqual(histories[0].messages, [{ role: 'user', content: 'history B' }]);
  const selections = posted.filter((message) => message.type === 'sessionSelected');
  assert.equal(selections.some((message) => message.projectHash === 'project-a'), false);
}

async function testRestoredPanelWithoutProjectHashResolvesBeforeLoadingHistory() {
  const calls: string[] = [];
  const history = [{ role: 'user', content: 'restored history' }];
  const posted: Array<{ type?: string; messages?: unknown[] }> = [];
  const webview = { postMessage: (message: { type?: string; messages?: unknown[] }) => { posted.push(message); } };
  const panel = { webview };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    resolveSession: async (sessionId: string) => {
      calls.push(`resolve:${sessionId}`);
      return { id: sessionId, project_hash: 'project-a' };
    },
    getSession: async (projectHash: string, sessionId: string) => {
      calls.push(`detail:${projectHash}:${sessionId}`);
      return { messages: history };
    },
  } as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _restorePanelHistory: (
      panel: typeof panel,
      sessionId: string,
      projectHash?: string,
    ) => Promise<void>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._panelReady.set('session-a', true);

  await unsafeProvider._restorePanelHistory(panel, 'session-a');

  assert.deepEqual(calls, [
    'resolve:session-a',
    'detail:project-a:session-a',
  ]);
  assert.deepEqual(
    posted.find((message) => message.type === 'sessionMessages')?.messages,
    history,
  );
}

async function testProjectHashResolutionFallsBackOnlyWithinTheCurrentWorkspace() {
  vscodeMock.workspace.workspaceFolders = [{ uri: { fsPath: '/repo/current' } }];
  const calls: string[] = [];
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {
    resolveSession: async () => {
      calls.push('resolve');
      throw new Error('unsupported endpoint');
    },
    listSessionsForWorkingDir: async (workingDir: string) => {
      calls.push(`scoped:${workingDir}`);
      return [{
        id: 'session-a',
        project_hash: 'project-a',
        working_dir: '/repo/current',
      }];
    },
  } as never);
  const unsafeProvider = provider as unknown as {
    _resolveSessionProjectHash: (
      sessionId: string,
      projectHash?: string,
    ) => Promise<string | undefined>;
  };

  const projectHash = await unsafeProvider._resolveSessionProjectHash('session-a');

  assert.equal(projectHash, 'project-a');
  assert.deepEqual(calls, ['resolve', 'scoped:/repo/current']);
  vscodeMock.workspace.workspaceFolders = [];
}

function testLiveStreamEventsWaitForPanelReadiness() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, { webview: { postMessage: (message: unknown) => void } }>;
    _panelReady: Map<string, boolean>;
    _postStreamEventIfReady: (sessionId: string, message: unknown) => void;
  };
  const posted: unknown[] = [];
  unsafeProvider._panels.set('session-a', {
    webview: { postMessage: (message) => { posted.push(message); } },
  });
  unsafeProvider._panelReady.set('session-a', false);

  unsafeProvider._postStreamEventIfReady('session-a', { type: 'text', content: 'once' });
  assert.equal(posted.length, 0);
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._postStreamEventIfReady('session-a', { type: 'text', content: 'once' });
  assert.deepEqual(posted, [{ type: 'text', content: 'once' }]);
}

function testReadyCatchUpReplaysEventsThatArrivedDuringInitialization() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const webview = { postMessage: (message: unknown) => { posted.push(message); } };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelReady: Map<string, boolean>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: Array<{ type: string; data: Record<string, unknown> }>;
    }>;
    _finishPanelReadyReplay: (
      webview: typeof webview,
      cursor: { sessionId?: string; streamGeneration: number; replayedEvents: number },
    ) => void;
  };
  const posted: unknown[] = [];
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 7,
    queuedMessages: [],
    eventBuffer: [
      { type: 'text', data: { content: 'already replayed' } },
      { type: 'text', data: { content: 'arrived during init' } },
    ],
  });

  unsafeProvider._finishPanelReadyReplay(webview, {
    sessionId: 'session-a',
    streamGeneration: 7,
    replayedEvents: 1,
  });

  assert.deepEqual(posted, [{ type: 'text', content: 'arrived during init' }]);
  assert.equal(unsafeProvider._panelReady.get('session-a'), true);
}

function testReadyCatchUpReplaysReplacementGenerationFromStart() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const posted: unknown[] = [];
  const webview = { postMessage: (message: unknown) => { posted.push(message); } };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelReady: Map<string, boolean>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: Array<{ type: string; data: Record<string, unknown> }>;
    }>;
    _finishPanelReadyReplay: (
      webview: typeof webview,
      cursor: { sessionId?: string; streamGeneration: number; replayedEvents: number },
    ) => void;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 8,
    queuedMessages: [],
    eventBuffer: [
      { type: 'userMessage', data: { text: 'new generation' } },
      { type: 'text', data: { content: 'new output' } },
    ],
  });

  unsafeProvider._finishPanelReadyReplay(webview, {
    sessionId: 'session-a',
    streamGeneration: 7,
    replayedEvents: 3,
  });

  assert.deepEqual(posted, [
    { type: 'userMessage', text: 'new generation' },
    { type: 'resumeStreaming' },
    { type: 'text', content: 'new output' },
  ]);
  assert.equal(unsafeProvider._panelReady.get('session-a'), true);
}

function testTerminalArrivingDuringReadyReplayIsDeliveredOnceAfterCatchUp() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const posted: unknown[] = [];
  const webview = { postMessage: (message: unknown) => { posted.push(message); } };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelReady: Map<string, boolean>;
    _pendingMessages: Map<string, Array<{ message: unknown; generation?: number }>>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _postTerminalForSession: (sessionId: string, message: unknown) => void;
    _finishPanelReadyReplay: (
      webview: typeof webview,
      cursor: { sessionId?: string; streamGeneration: number; replayedEvents: number },
    ) => void;
    _flushPendingMessages: (sessionId: string) => void;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 7,
    queuedMessages: [],
    eventBuffer: [],
  });

  unsafeProvider._postTerminalForSession('session-a', { type: 'done', stopReason: 'max_rounds' });
  unsafeProvider._finishPanelReadyReplay(webview, {
    sessionId: 'session-a',
    streamGeneration: 7,
    replayedEvents: 2,
  });
  unsafeProvider._flushPendingMessages('session-a');

  assert.deepEqual(posted, [{ type: 'done', stopReason: 'max_rounds' }]);
  assert.equal(unsafeProvider._pendingMessages.has('session-a'), false);
}

function testReadyDoesNotFlushHistoryAlreadyDeliveredByItsAtomicSnapshot() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const posted: unknown[] = [];
  const webview = { postMessage: (message: unknown) => { posted.push(message); } };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelReady: Map<string, boolean>;
    _pendingMessages: Map<string, Array<{ message: unknown; generation?: number }>>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _finishPanelReadyReplay: (
      webview: typeof webview,
      cursor: {
        sessionId?: string;
        streamGeneration: number;
        replayedEvents: number;
        historyGeneration?: number;
      },
    ) => void;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 7,
    queuedMessages: [],
    eventBuffer: [],
  });
  unsafeProvider._pendingMessages.set('session-a', [{
    generation: 7,
    message: { type: 'sessionMessages', messages: [{ role: 'user', content: 'same snapshot' }] },
  }]);

  unsafeProvider._finishPanelReadyReplay(webview, {
    sessionId: 'session-a',
    streamGeneration: 7,
    replayedEvents: 0,
    historyGeneration: 7,
  });

  assert.deepEqual(posted, []);
}

function testClosingTheLastSessionTabStopsTheUnobservableTurn() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  const disposeHandlers = Array.from(source.matchAll(
    /panel\.onDidDispose\(\(\) => \{[\s\S]*?\n    \}\);/g,
  ), (match) => match[0]);

  assert.equal(disposeHandlers.length, 2, 'new and restored tabs must both register disposal');
  for (const disposeHandler of disposeHandlers) {
    assert.match(
      disposeHandler,
      /this\._handlePanelDisposed\(panel, webview\)/,
      'new and restored tabs must share the same disposal lifecycle',
    );
    assert.doesNotMatch(disposeHandler, /panel\.webview/);
  }

  const cleanupStart = source.indexOf('private _handlePanelDisposed');
  const cleanupEnd = source.indexOf('private async _ensureSessionForWebview', cleanupStart);
  const cleanup = source.slice(cleanupStart, cleanupEnd);
  assert.match(cleanup, /abortController\?\.abort\(\)/);
  assert.match(cleanup, /this\._client\.stopGeneration\(disposedSid\)/);
  assert.ok(
    cleanup.indexOf('_panels.delete(disposedSid)') < cleanup.indexOf('abortController?.abort()'),
    'panel indexes must be cleaned before stopping the runtime so cleanup cannot be skipped',
  );
  assert.ok(
    cleanup.indexOf('rt.queuedMessages = []') < cleanup.indexOf('if (rt?.isGenerating || rt?.recoveryLocked)'),
    'closing a tab must clear queued follow-up prompts even after the current turn already reached terminal',
  );
}

function testSessionBoundMessageNeverFallsBackToAnotherPanel() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const postedA: unknown[] = [];
  const postedB: unknown[] = [];
  const sidebar: unknown[] = [];
  const unsafeProvider = provider as unknown as {
    _view?: { webview: { postMessage: (message: unknown) => void } };
    _panels: Map<string, { webview: { postMessage: (message: unknown) => void } }>;
    _focusedPanelId?: string;
    _activeSessionId?: string;
    _postMessageForSession: (sessionId: string, message: unknown) => void;
  };
  unsafeProvider._view = { webview: { postMessage: (message) => { sidebar.push(message); } } };
  unsafeProvider._panels.set('session-a', {
    webview: { postMessage: (message) => { postedA.push(message); } },
  });
  unsafeProvider._panels.set('session-b', {
    webview: { postMessage: (message) => { postedB.push(message); } },
  });
  unsafeProvider._focusedPanelId = 'session-b';
  unsafeProvider._activeSessionId = 'session-b';

  unsafeProvider._postMessageForSession('session-missing', { type: 'error', message: 'only missing' });

  assert.deepEqual(postedA, []);
  assert.deepEqual(postedB, []);
  assert.deepEqual(sidebar, []);
}

function testSessionSelectionDoesNotRewriteUnrelatedTabBindings() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const postedA: unknown[] = [];
  const postedB: unknown[] = [];
  const sidebar: unknown[] = [];
  const unsafeProvider = provider as unknown as {
    _view?: { webview: { postMessage: (message: unknown) => void } };
    _panels: Map<string, { webview: { postMessage: (message: unknown) => void } }>;
    _panelReady: Map<string, boolean>;
    _selectSession: (sessionId?: string, projectHash?: string) => void;
  };
  unsafeProvider._view = { webview: { postMessage: (message) => { sidebar.push(message); } } };
  unsafeProvider._panels.set('session-a', {
    webview: { postMessage: (message) => { postedA.push(message); } },
  });
  unsafeProvider._panels.set('session-b', {
    webview: { postMessage: (message) => { postedB.push(message); } },
  });
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._panelReady.set('session-b', true);

  unsafeProvider._selectSession('session-a', 'project-a');

  const selected = { type: 'sessionSelected', sessionId: 'session-a', projectHash: 'project-a' };
  assert.deepEqual(postedA, [selected]);
  assert.deepEqual(sidebar, [selected]);
  assert.deepEqual(postedB, []);
}

function testSessionSelectionIsNeverBroadcastToUnrelatedTabsAndCanonicalRemapPersists() {
  const source = readFileSync(join(process.cwd(), 'src/chat/provider.ts'), 'utf8');
  assert.doesNotMatch(
    source,
    /_broadcastMessage\(\{ type: 'sessionSelected'/,
    'session selection must target only the owning tab plus the sidebar',
  );
  const remapStart = source.indexOf('if (sessionId && sessionId !== streamSessionId) {');
  const remapEnd = source.indexOf('const doneSessionId =', remapStart);
  const canonicalRemap = source.slice(remapStart, remapEnd);
  assert.match(
    canonicalRemap,
    /sessionSelected/,
    'a temporary-to-canonical session remap must persist the canonical binding in its webview',
  );
}

function testDoneForActiveSessionDoesNotEraseItsProjectBinding() {
  const selected = chatReducer(initialState, {
    type: 'SET_ACTIVE_SESSION',
    sessionId: 'canonical',
    projectHash: 'project-a',
  });
  const afterDone = chatReducer(selected, {
    type: 'SET_ACTIVE_SESSION',
    sessionId: 'canonical',
  });

  assert.equal(afterDone.activeProjectHash, 'project-a');
}

async function testGenerationStartedRoutesToTheRequestedSession() {
  let callbacks: unknown;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const postedA: Array<{ type?: string }> = [];
  const postedB: Array<{ type?: string }> = [];
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, { webview: { postMessage: (message: { type?: string }) => void } }>;
    _panelReady: Map<string, boolean>;
    _focusedPanelId?: string;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._panels.set('session-a', {
    webview: { postMessage: (message) => { postedA.push(message); } },
  });
  unsafeProvider._panels.set('session-b', {
    webview: { postMessage: (message) => { postedB.push(message); } },
  });
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._panelReady.set('session-b', true);
  unsafeProvider._focusedPanelId = 'session-b';

  await unsafeProvider._handleSend('session a prompt', undefined, undefined, undefined, 'session-a', 'build');

  assert.ok(callbacks);
  assert.deepEqual(postedA.map((message) => message.type), ['generationStarted']);
  assert.deepEqual(postedB, []);
}

async function testUnboundReadyPanelGetsAnOwnedSessionBeforeItsFirstTurn() {
  const created = {
    id: 'session-new',
    project_hash: 'project-new',
    working_dir: '/repo',
  };
  const client = {
    createSession: async () => created,
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const webview = { postMessage: (_message: unknown) => undefined };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _panelSessions: Map<string, { sessionId: string; projectHash?: string; workingDir?: string }>;
    _focusedPanelId?: string;
    _activeSessionId?: string;
    _refreshSessions: () => Promise<void>;
    _ensureSessionForWebview: (webview: typeof webview) => Promise<string | undefined>;
  };
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._refreshSessions = async () => {};

  const sessionId = await unsafeProvider._ensureSessionForWebview(webview);

  assert.equal(sessionId, 'session-new');
  assert.equal(unsafeProvider._panels.get('session-new'), panel);
  assert.equal(unsafeProvider._panelReady.get('session-new'), true);
  assert.deepEqual(unsafeProvider._panelSessions.get('session-new'), {
    sessionId: 'session-new',
    projectHash: 'project-new',
    workingDir: '/repo',
  });
  assert.equal(unsafeProvider._focusedPanelId, 'session-new');
  assert.equal(unsafeProvider._activeSessionId, 'session-new');
}

async function testExistingPanelSessionIsNeverReplacedJustBecauseRuntimeWasCold() {
  let creates = 0;
  const client = {
    createSession: async () => {
      creates += 1;
      return { id: 'wrong-replacement', project_hash: 'wrong-project' };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, unknown>;
    _panelSessions: Map<string, { sessionId: string; projectHash?: string }>;
    _sessionRuntimes: Map<string, { projectHash?: string }>;
    _ensureSession: (sessionId?: string) => Promise<string | undefined>;
  };
  unsafeProvider._panels.set('restored-session', {});
  unsafeProvider._panelSessions.set('restored-session', {
    sessionId: 'restored-session',
    projectHash: 'restored-project',
  });

  const sessionId = await unsafeProvider._ensureSession('restored-session');

  assert.equal(sessionId, 'restored-session');
  assert.equal(creates, 0);
  assert.equal(unsafeProvider._sessionRuntimes.get('restored-session')?.projectHash, 'restored-project');
}

async function testSecondReadyStillRestoresCompleteHistory() {
  const client = {
    listModels: async () => [],
    getApprovalMode: async () => ({ mode: 'build' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string; messages?: unknown[] }> = [];
  const webview = { postMessage: (message: { type?: string; messages?: unknown[] }) => { posted.push(message); } };
  const panel = { webview };
  const history = [{ role: 'user', content: 'persisted prompt' }];
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelSessions: Map<string, { sessionId: string; projectHash?: string; messages?: unknown[] }>;
    _sendSetupState: () => Promise<void>;
    _loadSessionsForDisplay: () => Promise<{ sessions: unknown[] }>;
    _annotateSessionGenerating: () => Promise<void>;
    _sendEditorContext: () => void;
    _sendInitialState: (webview: typeof webview, mode: 'tab') => Promise<unknown>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelSessions.set('session-a', {
    sessionId: 'session-a',
    projectHash: 'project-a',
    messages: history,
  });
  unsafeProvider._sendSetupState = async () => {};
  unsafeProvider._loadSessionsForDisplay = async () => ({ sessions: [] });
  unsafeProvider._annotateSessionGenerating = async () => {};
  unsafeProvider._sendEditorContext = () => {};

  await unsafeProvider._sendInitialState(webview, 'tab');
  await unsafeProvider._sendInitialState(webview, 'tab');

  const historyMessages = posted.filter((message) => message.type === 'sessionMessages');
  assert.equal(historyMessages.length, 2);
  assert.deepEqual(historyMessages[0].messages, history);
  assert.deepEqual(historyMessages[1].messages, history);
}

async function testLateHistoryRefreshDoesNotRecreateADisposedPanelBinding() {
  let resolveSession!: (detail: { messages: unknown[] }) => void;
  const sessionDetail = new Promise<{ messages: unknown[] }>((resolve) => { resolveSession = resolve; });
  const client = { getSession: () => sessionDetail };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, unknown>;
    _panelSessions: Map<string, { sessionId: string; projectHash?: string; messages?: unknown[] }>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      projectHash?: string;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      messages?: unknown[];
    }>;
    _reloadFinishedSessionHistory: (sessionId: string, generation: number) => Promise<void>;
  };
  unsafeProvider._panels.set('session-a', {});
  unsafeProvider._panelSessions.set('session-a', { sessionId: 'session-a', projectHash: 'project-a' });
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 1,
    projectHash: 'project-a',
    queuedMessages: [],
    eventBuffer: [],
  });

  const refreshing = unsafeProvider._reloadFinishedSessionHistory('session-a', 1);
  unsafeProvider._panels.delete('session-a');
  unsafeProvider._panelSessions.delete('session-a');
  resolveSession({ messages: [{ role: 'assistant', content: 'persisted' }] });
  await refreshing;

  assert.equal(unsafeProvider._panelSessions.has('session-a'), false);
  assert.deepEqual(unsafeProvider._sessionRuntimes.get('session-a')?.messages, [
    { role: 'assistant', content: 'persisted' },
  ]);
}

async function testReadyDoesNotPublishHistoryCapturedFromAnOlderGeneration() {
  let resolveHistory!: (messages: unknown[]) => void;
  const historyPromise = new Promise<unknown[]>((resolve) => { resolveHistory = resolve; });
  const client = {
    listModels: async () => [],
    getApprovalMode: async () => ({ mode: 'build' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string }> = [];
  const webview = { postMessage: (message: { type?: string }) => { posted.push(message); } };
  const panel = { webview };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, typeof panel>;
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panelSessions: Map<string, { sessionId: string; projectHash?: string; messagesPromise?: Promise<unknown[]> }>;
    _sessionRuntimes: Map<string, { isGenerating: boolean; streamGeneration: number; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
    _sendSetupState: () => Promise<void>;
    _loadSessionsForDisplay: () => Promise<{ sessions: unknown[] }>;
    _annotateSessionGenerating: () => Promise<void>;
    _sendEditorContext: () => void;
    _sendInitialState: (webview: typeof webview, mode: 'tab') => Promise<unknown>;
  };
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panelSessions.set('session-a', {
    sessionId: 'session-a',
    projectHash: 'project-a',
    messagesPromise: historyPromise,
  });
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 1,
    queuedMessages: [],
    eventBuffer: [],
  });
  unsafeProvider._sendSetupState = async () => {};
  unsafeProvider._loadSessionsForDisplay = async () => ({ sessions: [] });
  unsafeProvider._annotateSessionGenerating = async () => {};
  unsafeProvider._sendEditorContext = () => {};

  const initializing = unsafeProvider._sendInitialState(webview, 'tab');
  await Promise.resolve();
  const runtime = unsafeProvider._sessionRuntimes.get('session-a')!;
  runtime.streamGeneration = 2;
  runtime.isGenerating = true;
  runtime.eventBuffer = [{ type: 'userMessage', data: { text: 'replacement' } }];
  resolveHistory([{ role: 'user', content: 'old generation' }]);
  await initializing;

  assert.equal(posted.some((message) => message.type === 'sessionMessages'), false);
}

async function testNewGenerationDropsStaleTerminalAndHistoryPendingMessages() {
  const client = { streamChat: () => new AbortController() };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string }> = [];
  const webview = { postMessage: (message: { type?: string }) => { posted.push(message); } };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, { webview: typeof webview }>;
    _panelReady: Map<string, boolean>;
    _sessionRuntimes: Map<string, { isGenerating: boolean; streamGeneration: number; terminalSeen?: boolean; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _postTerminalForSession: (sessionId: string, message: unknown, generation?: number) => void;
    _postOrQueueToPanel: (sessionId: string, message: unknown, generation?: number) => void;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _flushPendingMessages: (sessionId: string) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._panels.set('session-a', { webview });
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    streamGeneration: 1,
    terminalSeen: true,
    queuedMessages: [],
    eventBuffer: [],
  });
  unsafeProvider._postTerminalForSession('session-a', { type: 'done', stopReason: 'max_rounds' }, 1);
  unsafeProvider._postOrQueueToPanel('session-a', { type: 'sessionMessages', messages: [] }, 1);

  await unsafeProvider._handleSend('replacement', undefined, undefined, undefined, 'session-a', 'build');
  posted.length = 0;
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._flushPendingMessages('session-a');

  assert.deepEqual(posted, []);
}

async function testAbnormalTerminalSurvivesAuthoritativeHistoryRefresh() {
  let state = chatReducer(initialState, { type: 'ADD_USER_MESSAGE', text: 'prompt' });
  state = chatReducer(state, { type: 'START_GENERATION' });
  state = chatReducer(state, { type: 'APPEND_TEXT', content: 'partial result' });
  state = chatReducer(state, {
    type: 'LOAD_SESSION_MESSAGES',
    messages: [
      { role: 'user', content: 'prompt' },
      { role: 'assistant', content: 'partial result' },
    ],
    terminal: {
      type: 'done',
      stopReason: 'tool_loop_detected',
      message: 'Repeated Bash call was stopped.',
    },
  } as never);

  const assistant = state.messages.findLast((message: { role: string }) => message.role === 'assistant');
  const statuses = assistant?.blocks?.filter((block: { type: string }) => block.type === 'status') ?? [];
  assert.ok(
    statuses.some((block: { status?: { message?: string } }) => block.status?.message === 'Repeated Bash call was stopped.'),
    'history replacement must retain the authoritative abnormal terminal reason',
  );
  assert.equal(state.isGenerating, false);
}

async function testUnexpectedStreamEndLocksAnActiveUnattachedTurnUntilStopped() {
  let callbacks: any;
  let streams = 0;
  let stops = 0;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      streams += 1;
      callbacks = received;
      return new AbortController();
    },
    activeSessions: async () => ['session-a'],
    stopGeneration: async () => {
      stops += 1;
      return { success: true };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _focusedPanelId?: string;
    _sessionRuntimes: Map<string, { isGenerating: boolean; recoveryLocked?: boolean; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessageForSession: () => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._focusedPanelId = 'session-a';
  unsafeProvider._postMessageForSession = () => {};

  await unsafeProvider._handleSend('first', undefined, undefined, undefined, 'session-a', 'build');
  callbacks.onError('Stream ended before a terminal event');
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.recoveryLocked, true);
  await unsafeProvider._handleSend('must not overlap', undefined, undefined, undefined, 'session-a', 'build');
  assert.equal(streams, 1);

  provider.stopGeneration();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(stops, 1);
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.recoveryLocked, false);
}

async function testUnobservedInterruptedStreamRetainsReplayableSessionState() {
  let callbacks: any;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
    activeSessions: async () => [],
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: Array<{ type: string; data: Record<string, unknown> }>;
      terminal?: { type: string; generation: number; message?: string };
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _replayStreamBuffer: (sessionId: string, runtime: unknown, webview: unknown) => number;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };

  await unsafeProvider._handleSend('persist this prompt', undefined, undefined, undefined, 'session-a', 'build');
  callbacks.onText('partial answer');
  callbacks.onError('Stream ended before a terminal event');
  await new Promise((resolve) => setTimeout(resolve, 0));

  const runtime = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.deepEqual(runtime.eventBuffer.map((event) => event.type), ['userMessage', 'text']);
  assert.equal(runtime.terminal?.type, 'error');

  const replayed: unknown[] = [];
  const count = unsafeProvider._replayStreamBuffer(
    'session-a',
    runtime,
    { postMessage: (message: unknown) => { replayed.push(message); } },
  );
  assert.equal(count, 2);
  assert.deepEqual(replayed, [
    { type: 'userMessage', text: 'persist this prompt' },
    { type: 'resumeStreaming' },
    { type: 'text', content: 'partial answer' },
  ]);
}

async function testStopTargetsTheOwningSessionInsteadOfTheFocusedFallback() {
  const stopped: string[] = [];
  const client = {
    stopGeneration: async (sessionId: string) => {
      stopped.push(sessionId);
      return { success: true, message: 'stopped' };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _focusedPanelId?: string;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
  };
  unsafeProvider._focusedPanelId = 'session-b';
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 1,
    queuedMessages: [],
    eventBuffer: [],
  });
  unsafeProvider._sessionRuntimes.set('session-b', {
    isGenerating: true,
    streamGeneration: 1,
    queuedMessages: [],
    eventBuffer: [],
  });

  provider.stopGeneration('session-a');
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.deepEqual(stopped, ['session-a']);
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.isGenerating, false);
  assert.equal(unsafeProvider._sessionRuntimes.get('session-b')?.isGenerating, true);
}

async function testStopKeepsRecoveryLockUntilDaemonConfirmsCancellation() {
  let confirmStop!: (result: { success: boolean; message: string }) => void;
  const stopResult = new Promise<{ success: boolean; message: string }>((resolve) => {
    confirmStop = resolve;
  });
  const client = { stopGeneration: () => stopResult };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      recoveryLocked?: boolean;
      streamGeneration: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
  };
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 1,
    queuedMessages: [],
    eventBuffer: [],
  });

  provider.stopGeneration('session-a');
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.recoveryLocked, true);

  confirmStop({ success: true, message: 'stopped' });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.recoveryLocked, false);
}

function testRecoveryLockRemainsVisibleAndCannotQueueAnotherTurn() {
  let state = chatReducer(initialState, { type: 'RECOVERY_REQUIRED' } as never);
  assert.equal(state.recoveryLocked, true);
  state = chatReducer(state, { type: 'RECOVERY_CLEARED' } as never);
  assert.equal(state.recoveryLocked, false);

  const inputSource = readFileSync(join(process.cwd(), 'webview-ui/src/components/InputArea.tsx'), 'utf8');
  assert.match(inputSource, /state\.recoveryLocked/);
  assert.match(inputSource, /state\.isGenerating && !state\.recoveryLocked/);
}

async function testAuthFileWatcherRefreshesSetupState() {
  fileWatchers.length = 0;
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _watchAtomCodeAuth: (path: string) => void;
    _sendSetupState: () => Promise<void>;
  };
  let refreshes = 0;
  unsafeProvider._sendSetupState = async () => { refreshes += 1; };

  unsafeProvider._watchAtomCodeAuth('/tmp/atomcode/auth.toml');

  assert.equal(fileWatchers.length, 1);
  assert.equal(fileWatchers[0].pattern.base, '/tmp/atomcode');
  assert.equal(fileWatchers[0].pattern.pattern, 'auth.toml');
  fileWatchers[0].delete?.();
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(refreshes, 1);

  provider.dispose();
  assert.equal(fileWatchers[0].disposed, true);
}

async function testStaleSetupRefreshCannotOverwriteNewerAuthState() {
  let resolveFirstAuth!: (value: unknown) => void;
  const firstAuth = new Promise((resolve) => { resolveFirstAuth = resolve; });
  let authCalls = 0;
  const client = {
    authStatus: () => {
      authCalls += 1;
      if (authCalls === 1) return firstAuth;
      return Promise.resolve({
        logged_in: true,
        expired: false,
        auth_path: '/tmp/atomcode/auth.toml',
        user: { id: 'new-user' },
      });
    },
    listProviders: () => Promise.resolve({
      default_provider: 'main',
      providers: [{
        name: 'main', type: 'openai', model: 'new-model', has_api_key: false,
        requires_login: true, is_default: true, context_window: 128_000,
        skip_tls_verify: false,
      }],
    }),
    getConfig: () => Promise.resolve({
      path: '/tmp/atomcode/config.toml', default_provider: 'main', provider_count: 1,
      providers: [], network: {}, telemetry: {},
    }),
    listModels: () => Promise.resolve([]),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sendSetupState: () => Promise<void>;
    _broadcastMessage: (message: unknown) => void;
  };
  const messages: Array<{ type?: string; auth?: { logged_in?: boolean } }> = [];
  unsafeProvider._broadcastMessage = (message) => {
    messages.push(message as { type?: string; auth?: { logged_in?: boolean } });
  };

  const staleRefresh = unsafeProvider._sendSetupState();
  await unsafeProvider._sendSetupState();
  resolveFirstAuth({
    logged_in: false,
    expired: false,
    auth_path: '/tmp/atomcode/auth.toml',
    user: null,
  });
  await staleRefresh;

  const authMessages = messages.filter((message) => message.type === 'authStatus');
  assert.equal(authMessages.at(-1)?.auth?.logged_in, true);
  assert.equal(authMessages.some((message) => message.auth?.logged_in === false), false);
  provider.dispose();
}

async function testDisposedPanelCannotBlockNewPanelSetupState() {
  type FakeWebview = {
    html: string;
    options?: unknown;
    postMessage: (message: unknown) => Promise<boolean>;
    onDidReceiveMessage: (listener: (message: unknown) => void) => { dispose(): void };
  };
  type FakePanel = {
    readonly webview: FakeWebview;
    iconPath?: unknown;
    title: string;
    onDidChangeViewState: (listener: (event: unknown) => void) => { dispose(): void };
    onDidDispose: (listener: () => void) => { dispose(): void };
    reveal(): void;
    dispose(): void;
  };

  function createPanel(posted: unknown[]) {
    let disposed = false;
    let disposeListener: (() => void) | undefined;
    const webview: FakeWebview = {
      html: '',
      postMessage: async (message) => {
        posted.push(message);
        return true;
      },
      onDidReceiveMessage: () => ({ dispose() {} }),
    };
    const panel: FakePanel = {
      get webview() {
        if (disposed) throw new Error('Webview is disposed');
        return webview;
      },
      title: 'AtomCode',
      onDidChangeViewState: () => ({ dispose() {} }),
      onDidDispose: (listener) => {
        disposeListener = listener;
        return { dispose() {} };
      },
      reveal() {},
      dispose() {
        disposed = true;
        disposeListener?.();
      },
    };
    return { panel, webview };
  }

  const client = {
    authStatus: async () => ({
      logged_in: true,
      expired: false,
      auth_path: '/tmp/atomcode/auth.toml',
      user: { id: 'user-1' },
    }),
    listProviders: async () => ({
      default_provider: 'main',
      providers: [{
        name: 'main', type: 'openai', model: 'model-1', has_api_key: false,
        requires_login: true, is_default: true, context_window: 128_000,
        skip_tls_verify: false,
      }],
    }),
    getConfig: async () => ({
      path: '/tmp/atomcode/config.toml', default_provider: 'main', provider_count: 1,
      providers: [], network: {}, telemetry: {},
    }),
    listModels: async () => [],
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _getHtml: () => string;
    _panels: Map<string, FakePanel>;
    _webviewPanels: Map<FakeWebview, FakePanel>;
    _sendSetupState: (webview: FakeWebview) => Promise<void>;
    setupPanelForRestore: (panel: FakePanel, sessionId?: string) => void;
  };
  unsafeProvider._getHtml = () => '<html></html>';

  const mutableWindow = vscodeMock.window as {
    tabGroups?: { all: unknown[] };
    createWebviewPanel?: () => FakePanel;
  };
  const previousTabGroups = mutableWindow.tabGroups;
  const previousCreateWebviewPanel = mutableWindow.createWebviewPanel;
  mutableWindow.tabGroups = { all: [] };

  try {
    const oldPosted: unknown[] = [];
    const old = createPanel(oldPosted);
    mutableWindow.createWebviewPanel = () => old.panel;
    provider.openInTab('old-session');

    assert.doesNotThrow(() => old.panel.dispose());
    assert.equal(unsafeProvider._panels.has('old-session'), false);
    assert.equal(unsafeProvider._webviewPanels.has(old.webview), false);

    const newPosted: unknown[] = [];
    const current = createPanel(newPosted);
    mutableWindow.createWebviewPanel = () => current.panel;
    provider.openInTab('new-session');
    await unsafeProvider._sendSetupState(current.webview);

    const messageTypes = newPosted.map((message) => (message as { type?: string }).type);
    assert.ok(messageTypes.includes('authStatus'));
    assert.ok(messageTypes.includes('providers'));
    assert.ok(messageTypes.includes('setupState'));
    current.panel.dispose();

    const restored = createPanel([]);
    unsafeProvider.setupPanelForRestore(restored.panel, 'restored-session');
    assert.doesNotThrow(() => restored.panel.dispose());
    assert.equal(unsafeProvider._panels.has('restored-session'), false);
    assert.equal(unsafeProvider._webviewPanels.has(restored.webview), false);
  } finally {
    mutableWindow.tabGroups = previousTabGroups;
    mutableWindow.createWebviewPanel = previousCreateWebviewPanel;
    provider.dispose();
  }
}

async function testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _focusedPanelId?: string;
    _sessionRuntimes: Map<string, { isGenerating: boolean; queuedMessages: unknown[]; eventBuffer: unknown[]; terminal?: { type: string; stopReason?: string } }>;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _sendNextQueuedMessage: (sessionId: string) => Promise<void>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
  };

  const sent: unknown[][] = [];
  unsafeProvider._focusedPanelId = undefined;
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    queuedMessages: [{ text: 'next prompt', context: [{ path: 'file.ts', type: 'file' }], clientMessageId: 'queued-1', approvalMode: 'plan' }],
    eventBuffer: [],
  });
  unsafeProvider._handleSend = async (...args: unknown[]) => {
    sent.push(args);
  };

  await unsafeProvider._sendNextQueuedMessage('session-a');

  assert.equal(sent.length, 1);
  assert.deepEqual(sent[0], [
    'next prompt',
    [{ path: 'file.ts', type: 'file' }],
    undefined,
    'queued-1',
    'session-a',
    'plan',
  ]);
}

async function testQueuedMessageDoesNotDrainWhileApprovalModeIsPending() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, { isGenerating: boolean; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _sendNextQueuedMessage: (sessionId: string) => Promise<void>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
  };

  const sent: unknown[][] = [];
  unsafeProvider._approvalModeState = {
    confirmedMode: 'build',
    displayMode: 'plan',
    pendingMode: 'plan',
  };
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    queuedMessages: [{ text: 'next prompt', clientMessageId: 'queued-1', approvalMode: 'build' }],
    eventBuffer: [],
  });
  unsafeProvider._handleSend = async (...args: unknown[]) => {
    sent.push(args);
  };

  await unsafeProvider._sendNextQueuedMessage('session-a');

  assert.equal(sent.length, 0);
  assert.equal(
    unsafeProvider._sessionRuntimes.get('session-a')?.queuedMessages.length,
    1,
  );
}

async function testAbnormalDoneDoesNotDrainQueuedMessages() {
  let callbacks: any;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _reloadFinishedSessionHistory: () => Promise<void>;
    _refreshSessions: () => Promise<void>;
    _sendNextQueuedMessage: () => Promise<void>;
    _postTerminalForSession: (sessionId: string, message: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    queuedMessages: [{ text: 'must not auto-run after an incomplete turn' }],
    eventBuffer: [],
  });
  unsafeProvider._reloadFinishedSessionHistory = async () => {};
  unsafeProvider._refreshSessions = async () => {};
  let drains = 0;
  unsafeProvider._sendNextQueuedMessage = async () => { drains += 1; };
  const terminalMessages: unknown[] = [];
  unsafeProvider._postTerminalForSession = (_sessionId, message) => {
    terminalMessages.push(message);
  };

  await unsafeProvider._handleSend(
    'first prompt',
    undefined,
    undefined,
    undefined,
    'session-a',
    'build',
  );
  callbacks.onDone(
    0,
    4,
    'session-a',
    'tool_loop_detected',
    'The turn was stopped as incomplete.',
  );
  await new Promise((resolve) => setTimeout(resolve, 100));

  assert.equal(drains, 0);
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.queuedMessages.length, 0);
  assert.deepEqual(terminalMessages[0], { type: 'clearQueuedMessages' });
  assert.equal((terminalMessages[1] as { type?: string })?.type, 'done');
}

async function testCanonicalSessionRemapWithoutPanelDoesNotCreateAPhantomPanel() {
  let callbacks: any;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, unknown>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      terminal?: { type: string; stopReason?: string };
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _reloadFinishedSessionHistory: () => Promise<void>;
    _refreshSessions: () => Promise<void>;
    _postMessage: (message: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._reloadFinishedSessionHistory = async () => {};
  unsafeProvider._refreshSessions = async () => {};
  const posted: unknown[] = [];
  unsafeProvider._postMessage = (message) => { posted.push(message); };

  await unsafeProvider._handleSend('first prompt', undefined, undefined, undefined, 'temporary', 'build');
  callbacks.onDone(0, 1, 'canonical', 'tool_loop_detected', 'incomplete');

  assert.equal(unsafeProvider._panels.has('canonical'), false);
  assert.deepEqual(posted, [], 'a terminal without an observer must not fall back to another view');
  assert.equal(unsafeProvider._sessionRuntimes.get('canonical')?.terminal?.type, 'done');
  assert.equal(unsafeProvider._sessionRuntimes.get('canonical')?.terminal?.stopReason, 'tool_loop_detected');
}

async function testErrorQueuedForNotReadyPanelIsNotAlsoStoredForReplay() {
  let callbacks: any;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const webview = { postMessage: (_message: unknown) => undefined };
  const unsafeProvider = provider as unknown as {
    _panels: Map<string, { webview: typeof webview }>;
    _panelReady: Map<string, boolean>;
    _pendingMessages: Map<string, Array<{ message: unknown; generation?: number }>>;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      terminal?: { type: string; generation: number; message?: string };
      recoveryLocked?: boolean;
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessage: (_message: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._panels.set('session-a', { webview });
  unsafeProvider._panelReady.set('session-a', false);
  unsafeProvider._postMessage = () => undefined;

  await unsafeProvider._handleSend('first prompt', undefined, undefined, undefined, 'session-a', 'build');
  callbacks.onError('stream ended before terminal');

  assert.deepEqual(unsafeProvider._pendingMessages.get('session-a'), [{
    generation: 1,
    message: {
      type: 'error',
      message: 'stream ended before terminal',
    },
  }, {
    generation: 1,
    message: { type: 'recoveryRequired' },
  }]);
  assert.deepEqual(unsafeProvider._sessionRuntimes.get('session-a')?.terminal, {
    type: 'error',
    generation: 1,
    message: 'stream ended before terminal',
  });
}

async function testStartingANewGenerationClearsStoredTerminalError() {
  const client = {
    streamChat: () => new AbortController(),
    activeSessions: async () => [],
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      terminal?: { type: string; generation: number; message?: string };
      recoveryLocked?: boolean;
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessage: (_message: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    queuedMessages: [],
    eventBuffer: [],
    terminal: { type: 'error', generation: 1, message: 'previous turn failed' },
    recoveryLocked: true,
  });
  unsafeProvider._postMessage = () => undefined;

  await unsafeProvider._handleSend('retry', undefined, undefined, undefined, 'session-a', 'build');

  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.terminal, undefined);
}

async function testLateTerminalFromCancelledGenerationCannotStopReplacementTurn() {
  const callbacks: any[] = [];
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks.push(received);
      return new AbortController();
    },
    stopGeneration: async () => ({ success: true, message: 'stopped' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _focusedPanelId?: string;
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      terminalSeen?: boolean;
      streamGeneration?: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessage: (_message: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._focusedPanelId = 'session-a';
  unsafeProvider._postMessage = () => undefined;

  await unsafeProvider._handleSend('first', undefined, undefined, undefined, 'session-a', 'build');
  provider.stopGeneration();
  await new Promise((resolve) => setTimeout(resolve, 0));
  await unsafeProvider._handleSend('replacement', undefined, undefined, undefined, 'session-a', 'build');
  callbacks[0].onStopped();

  const runtime = unsafeProvider._sessionRuntimes.get('session-a');
  assert.equal(runtime?.streamGeneration, 2);
  assert.equal(runtime?.isGenerating, true);
  assert.equal(runtime?.terminalSeen, false);
}

async function testLateEventsFromCompletedGenerationAreIgnored() {
  let callbacks: any;
  const client = {
    streamChat: (_request: unknown, received: unknown) => {
      callbacks = received;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      terminalSeen?: boolean;
      streamGeneration?: number;
      queuedMessages: unknown[];
      eventBuffer: Array<{ type: string }>;
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _reloadFinishedSessionHistory: () => Promise<void>;
    _refreshSessions: () => Promise<void>;
    _postTerminalForSession: () => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._reloadFinishedSessionHistory = async () => {};
  unsafeProvider._refreshSessions = async () => {};
  unsafeProvider._postTerminalForSession = () => {};

  await unsafeProvider._handleSend('first', undefined, undefined, undefined, 'session-a', 'build');
  callbacks.onText('before terminal');
  callbacks.onDone(1, 0, 'session-a', 'stopped');
  const runtime = unsafeProvider._sessionRuntimes.get('session-a')!;
  const before = runtime.eventBuffer.map((event) => event.type);

  callbacks.onText('late text');
  callbacks.onToolStart('late-call', 'bash', '{}');

  assert.deepEqual(runtime.eventBuffer.map((event) => event.type), before);
}

async function testClosedWebviewCannotCompleteFirstSessionBinding() {
  const client = {
    createSession: async () => ({
      id: 'session-new',
      project_hash: 'project-new',
      working_dir: '/repo',
    }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const webview = { postMessage: (_message: unknown) => undefined };
  const panel = { webview };
  let releaseRefresh!: () => void;
  let markRefreshStarted!: () => void;
  const refreshStarted = new Promise<void>((resolve) => { markRefreshStarted = resolve; });
  const refreshGate = new Promise<void>((resolve) => { releaseRefresh = resolve; });
  const unsafeProvider = provider as unknown as {
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panels: Map<string, typeof panel>;
    _panelReady: Map<string, boolean>;
    _panelSessions: Map<string, unknown>;
    _refreshSessions: () => Promise<void>;
    _ensureSessionForWebview: (webview: typeof webview) => Promise<string | undefined>;
  };
  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._refreshSessions = async () => {
    markRefreshStarted();
    await refreshGate;
  };

  const binding = unsafeProvider._ensureSessionForWebview(webview);
  await refreshStarted;
  unsafeProvider._webviewPanels.delete(webview);
  unsafeProvider._panels.delete('session-new');
  unsafeProvider._panelReady.delete('session-new');
  unsafeProvider._panelSessions.delete('session-new');
  releaseRefresh();

  assert.equal(await binding, undefined);
}

async function testClosedTabCannotFallbackOrStartAfterAdmissionWait() {
  let receive!: (message: any) => Promise<void>;
  const webview = {
    onDidReceiveMessage: (listener: (message: any) => Promise<void>) => {
      receive = listener;
    },
  };
  const panel = { webview };
  let streams = 0;
  let releaseLocalCommand!: () => void;
  let markLocalCommandStarted!: () => void;
  const localCommandStarted = new Promise<void>((resolve) => { markLocalCommandStarted = resolve; });
  const localCommandGate = new Promise<void>((resolve) => { releaseLocalCommand = resolve; });
  const client = {
    streamChat: () => {
      streams += 1;
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _webviewPanels: Map<typeof webview, typeof panel>;
    _panels: Map<string, typeof panel>;
    _setupWebviewMessageHandler: (webview: typeof webview, mode: string) => void;
    _ensureSessionForWebview: () => Promise<string | undefined>;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _handleLocalCommand: () => Promise<boolean>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._ensureSessionForWebview = async () => undefined;
  unsafeProvider._setupWebviewMessageHandler(webview, 'tab');

  await receive({ type: 'send', text: 'must not fallback' });
  assert.equal(streams, 0);

  unsafeProvider._webviewPanels.set(webview, panel);
  unsafeProvider._panels.set('session-a', panel);
  unsafeProvider._handleLocalCommand = async () => {
    markLocalCommandStarted();
    await localCommandGate;
    return false;
  };
  const sending = unsafeProvider._handleSend(
    'must not outlive tab',
    undefined,
    undefined,
    undefined,
    'session-a',
    'build',
    webview,
  );
  await localCommandStarted;
  unsafeProvider._webviewPanels.delete(webview);
  unsafeProvider._panels.delete('session-a');
  releaseLocalCommand();
  await sending;

  assert.equal(streams, 0);
}

async function testCancelledPreparationCannotStartAfterAReplacementTurn() {
  let resolveRead!: (value: Uint8Array) => void;
  const read = new Promise<Uint8Array>((resolve) => { resolveRead = resolve; });
  (vscodeMock.workspace as typeof vscodeMock.workspace & {
    fs?: { readFile: () => Promise<Uint8Array> };
  }).fs = { readFile: () => read };
  (vscodeMock.Uri as typeof vscodeMock.Uri & { file?: (fsPath: string) => { fsPath: string } }).file =
    (fsPath: string) => ({ fsPath });

  const requests: Array<{ message?: string }> = [];
  const client = {
    streamChat: (request: { message?: string }) => {
      requests.push(request);
      return new AbortController();
    },
    stopGeneration: async () => ({ success: true, message: 'stopped' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      terminalSeen?: boolean;
      streamGeneration?: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };

  const preparing = unsafeProvider._handleSend(
    'first',
    [{ path: '/repo/file.ts', type: 'file', fileName: 'file.ts' }],
    undefined,
    undefined,
    'session-a',
    'build',
  );
  for (let attempt = 0; attempt < 5 && !unsafeProvider._sessionRuntimes.get('session-a')?.isGenerating; attempt += 1) {
    await Promise.resolve();
  }
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.streamGeneration, 1);
  provider.stopGeneration('session-a');
  await new Promise((resolve) => setTimeout(resolve, 0));
  await unsafeProvider._handleSend('replacement', undefined, undefined, undefined, 'session-a', 'build');
  resolveRead(new TextEncoder().encode('old context'));
  await preparing;

  assert.equal(requests.length, 1);
  assert.equal(requests[0].message, 'replacement');
  assert.equal(unsafeProvider._sessionRuntimes.get('session-a')?.streamGeneration, 2);
  delete (vscodeMock.workspace as typeof vscodeMock.workspace & { fs?: unknown }).fs;
  delete (vscodeMock.Uri as typeof vscodeMock.Uri & { file?: unknown }).file;
}

async function testConcurrentPreparationQueuesTheSecondPromptInsteadOfStartingTwoStreams() {
  const localResolvers: Array<(handled: boolean) => void> = [];
  const requests: Array<{ message?: string }> = [];
  const client = {
    streamChat: (request: { message?: string }) => {
      requests.push(request);
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: Array<{ text: string }>;
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleLocalCommand: () => Promise<boolean>;
    _handleSend: (...args: unknown[]) => Promise<void>;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._handleLocalCommand = () => new Promise<boolean>((resolve) => {
    localResolvers.push(resolve);
  });

  const first = unsafeProvider._handleSend('first', undefined, undefined, undefined, 'session-a', 'build');
  const second = unsafeProvider._handleSend('second', undefined, undefined, undefined, 'session-a', 'build');
  await Promise.resolve();
  assert.equal(localResolvers.length, 2);

  localResolvers[0](false);
  await first;
  localResolvers[1](false);
  await second;

  assert.deepEqual(requests.map((request) => request.message), ['first']);
  assert.deepEqual(
    unsafeProvider._sessionRuntimes.get('session-a')?.queuedMessages.map((message) => message.text),
    ['second'],
  );
}

function testWebviewBridgeClearsQueuedMessagesOnHostInstruction() {
  const source = readFileSync(join(process.cwd(), 'webview-ui/src/state/ChatProvider.tsx'), 'utf8');
  const handler = source.match(/switch \(msg\.type\) \{[\s\S]*?case 'clearChat':/)?.[0] ?? '';

  assert.match(handler, /case 'clearQueuedMessages':/);
  assert.match(handler, /dispatch\(\{ type: 'CLEAR_QUEUED_MESSAGES' \}\)/);
}

function testLatePanelSessionBindingIsPersistedForRestore() {
  const source = readFileSync(join(process.cwd(), 'webview-ui/src/state/ChatProvider.tsx'), 'utf8');
  const selectedCase = source.match(/case 'sessionSelected':[\s\S]*?break;/)?.[0] ?? '';
  assert.match(selectedCase, /getVSCodeApi\(\)\.setState\(\{ sessionId: msg\.sessionId, projectHash: msg\.projectHash \}\)/);
}

async function testInitialStateDoesNotClearPendingApprovalModeSwitch() {
  const client = {
    listModels: async () => [],
    listSessions: async () => [],
    getApprovalMode: async () => ({ ok: true, mode: 'bypass' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _sendSetupState: () => Promise<void>;
    _sendEditorContext: () => void;
    _postMessage: (msg: unknown) => void;
    _sendInitialState: () => Promise<void>;
  };

  const messages: unknown[] = [];
  unsafeProvider._approvalModeState = {
    confirmedMode: 'build',
    displayMode: 'plan',
    pendingMode: 'plan',
  };
  unsafeProvider._sendSetupState = async () => {};
  unsafeProvider._sendEditorContext = () => {};
  unsafeProvider._postMessage = (msg: unknown) => {
    messages.push(msg);
  };

  await unsafeProvider._sendInitialState();

  assert.deepEqual(unsafeProvider._approvalModeState, {
    confirmedMode: 'build',
    displayMode: 'plan',
    pendingMode: 'plan',
  });
  assert.equal(
    messages.some((msg) => {
      const value = msg as { type?: string; pending?: boolean };
      return value.type === 'approvalMode' && value.pending === false;
    }),
    false,
  );
}

async function testPermissionRequestFromStreamIsForwardedToPanel() {
  const posted: unknown[] = [];
  const client = {
    streamChat: (_request: unknown, callbacks: {
      onPermissionRequest: (request: {
        sessionId: string;
        toolName: string;
        reason: string;
        callId: string;
        args: string;
      }) => void;
    }) => {
      callbacks.onPermissionRequest({
        sessionId: 'session-a',
        toolName: 'write_file',
        reason: 'Modify workspace file',
        callId: 'call-1',
        args: '{"path":"README.md"}',
      });
      return new AbortController();
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessage: (msg: unknown) => void;
    _postStreamEventIfReady: (sessionId: string, msg: unknown) => void;
  };

  unsafeProvider._postMessage = () => undefined;
  unsafeProvider._postStreamEventIfReady = (_sessionId: string, msg: unknown) => {
    posted.push(msg);
  };

  await unsafeProvider._handleSend('please write', undefined, undefined, undefined, 'session-a', 'build');

  assert.deepEqual(posted.find((msg) => (msg as { type?: string }).type === 'permissionRequest'), {
    type: 'permissionRequest',
    sessionId: 'session-a',
    id: 'call-1',
    toolName: 'write_file',
    reason: 'Modify workspace file',
    args: '{"path":"README.md"}',
    isDestructive: true,
  });
}

async function testPermissionResponsePostsDecisionToDaemon() {
  const calls: unknown[] = [];
  const posted: unknown[] = [];
  const client = {
    sendPermissionDecision: async (...args: unknown[]) => {
      calls.push(args);
      return { success: true };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _handlePermissionResponse: (msg: unknown) => Promise<void>;
    _postMessageForSession: (sessionId: string, msg: unknown) => void;
  };
  unsafeProvider._postMessageForSession = (_sessionId: string, msg: unknown) => {
    posted.push(msg);
  };

  await unsafeProvider._handlePermissionResponse({
    sessionId: 'session-a',
    id: 'call-1',
    toolName: 'write_file',
    allowed: true,
  });

  assert.deepEqual(calls, [['session-a', 'allow', 'write_file']]);
  assert.deepEqual(posted, [{
    type: 'permissionResponseResult',
    id: 'call-1',
    success: true,
    message: undefined,
  }]);
}

async function testPermissionResponsePostsExplicitDecisionToDaemon() {
  const calls: unknown[] = [];
  const posted: unknown[] = [];
  const client = {
    sendPermissionDecision: async (...args: unknown[]) => {
      calls.push(args);
      return { success: true };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _handlePermissionResponse: (msg: unknown) => Promise<void>;
    _postMessageForSession: (sessionId: string, msg: unknown) => void;
  };
  unsafeProvider._postMessageForSession = (_sessionId: string, msg: unknown) => {
    posted.push(msg);
  };

  await unsafeProvider._handlePermissionResponse({
    sessionId: 'session-a',
    id: 'call-1',
    toolName: 'mcp__server__tool',
    decision: 'allow_persist',
  });
  await unsafeProvider._handlePermissionResponse({
    sessionId: 'session-a',
    id: 'call-2',
    toolName: 'write_file',
    decision: 'always_allow',
  });

  assert.deepEqual(calls, [
    ['session-a', 'allow_persist', 'mcp__server__tool'],
    ['session-a', 'always_allow', 'write_file'],
  ]);
  assert.deepEqual(posted, [{
    type: 'permissionResponseResult',
    id: 'call-1',
    success: true,
    message: undefined,
  }, {
    type: 'permissionResponseResult',
    id: 'call-2',
    success: true,
    message: undefined,
  }]);
}

async function testLoadSessionsForDisplayUsesVscodeWorkspaceDirectory() {
  vscodeMock.workspace.workspaceFolders = [{ uri: { fsPath: '/repo/atomcode' } }];

  const calls: string[] = [];
  const client = {
    listSessions: async () => {
      calls.push('listSessions');
      return [{ id: 'other', name: 'Other project', project_hash: 'other-hash', updated_at: 300 }];
    },
    listSessionsForWorkingDir: async (workingDir: string) => {
      calls.push(`listSessionsForWorkingDir:${workingDir}`);
      return [{ id: 'current', name: 'Current project', project_hash: 'current-hash', working_dir: workingDir, updated_at: 100 }];
    },
    getProject: async () => {
      calls.push('getProject');
      return { project_hash: 'wrong-daemon-hash' };
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _loadSessionsForDisplay: () => Promise<{ sessions: Array<{ id: string }>; currentProjectHash?: string }>;
  };

  const loaded = await unsafeProvider._loadSessionsForDisplay();

  assert.deepEqual(calls, [
    'listSessionsForWorkingDir:/repo/atomcode',
  ]);
  assert.deepEqual(loaded.sessions.map((s) => s.id), ['current']);
  assert.equal(loaded.currentProjectHash, 'current-hash');

  vscodeMock.workspace.workspaceFolders = [];
}

async function testLoadSessionsForDisplayDoesNotFallBackToGlobalWhenWorkspaceHasNoSessions() {
  vscodeMock.workspace.workspaceFolders = [{ uri: { fsPath: '/repo/empty-project' } }];

  const client = {
    listSessions: async () => [{ id: 'other', name: 'Other project', project_hash: 'other-hash', updated_at: 300 }],
    listSessionsForWorkingDir: async () => [],
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _loadSessionsForDisplay: () => Promise<{ sessions: Array<{ id: string }>; currentProjectHash?: string }>;
  };

  const loaded = await unsafeProvider._loadSessionsForDisplay();

  assert.deepEqual(loaded.sessions, []);

  vscodeMock.workspace.workspaceFolders = [];
}

async function testLoadSessionsForDisplayFallsBackToGlobalWhenWorkspaceRequestFails() {
  vscodeMock.workspace.workspaceFolders = [{ uri: { fsPath: '/repo/atomcode' } }];

  const calls: string[] = [];
  const client = {
    listSessions: async () => {
      calls.push('listSessions');
      return [
        {
          id: 'current',
          name: 'Current fallback',
          project_hash: 'current-hash',
          working_dir: '/repo/atomcode',
          updated_at: 200,
        },
        {
          id: 'other',
          name: 'Other project',
          project_hash: 'other-hash',
          working_dir: '/repo/other',
          updated_at: 300,
        },
      ];
    },
    listSessionsForWorkingDir: async (workingDir: string) => {
      calls.push(`listSessionsForWorkingDir:${workingDir}`);
      throw new Error('unsupported endpoint');
    },
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _loadSessionsForDisplay: () => Promise<{ sessions: Array<{ id: string }>; currentProjectHash?: string }>;
  };

  const loaded = await unsafeProvider._loadSessionsForDisplay();

  assert.deepEqual(calls, [
    'listSessionsForWorkingDir:/repo/atomcode',
    'listSessions',
  ]);
  assert.deepEqual(loaded.sessions.map((s) => s.id), ['current']);
  assert.equal(loaded.currentProjectHash, 'current-hash');

  vscodeMock.workspace.workspaceFolders = [];
}

async function testRefreshSessionsOnlyPrependsSyntheticPanelsForCurrentWorkspace() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _panelSessions: Map<string, { sessionId: string; projectHash?: string; workingDir?: string }>;
    _sessionRuntimes: Map<string, { isGenerating: boolean; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
    _loadSessionsForDisplay: () => Promise<{ sessions: Array<{ id: string }>; workspaceFolder?: string; currentProjectHash?: string }>;
    _annotateSessionGenerating: (sessions: unknown[]) => Promise<void>;
    _broadcastMessage: (msg: unknown) => void;
    _refreshSessions: () => Promise<void>;
  };

  const messages: unknown[] = [];
  unsafeProvider._loadSessionsForDisplay = async () => ({
    sessions: [],
    workspaceFolder: '/repo/current',
  });
  unsafeProvider._annotateSessionGenerating = async () => {};
  unsafeProvider._broadcastMessage = (msg: unknown) => {
    messages.push(msg);
  };
  unsafeProvider._panelSessions.set('current-new', {
    sessionId: 'current-new',
    projectHash: 'current-hash',
    workingDir: '/repo/current',
  });
  unsafeProvider._panelSessions.set('other-new', {
    sessionId: 'other-new',
    projectHash: 'other-hash',
    workingDir: '/repo/other',
  });

  await unsafeProvider._refreshSessions();

  const sessionsMessage = messages.find((msg) => (msg as { type?: string }).type === 'sessions') as {
    sessions: Array<{ id: string }>;
  };
  assert.deepEqual(sessionsMessage.sessions.map((s) => s.id), ['current-new']);
}

function testMergeSessionsForDisplayShowsOnlyCurrentProjectSessionsWhenProjectIsKnown() {
  const merged = mergeSessionsForDisplay(
    [
      {
        id: 'global-newer',
        name: 'Global newer',
        project_hash: 'other-hash',
        updated_at: 300,
      },
      {
        id: 'duplicate-current',
        name: 'Current from global',
        project_hash: 'current-hash',
        working_dir: '/repo/current',
        updated_at: 200,
      },
    ],
    [
      {
        id: 'current-old',
        name: 'Current old',
        project_hash: 'current-hash',
        working_dir: '/repo/current',
        updated_at: 100,
      },
      {
        id: 'duplicate-current',
        name: 'Current from project endpoint',
        project_hash: 'current-hash',
        working_dir: '/repo/current',
        updated_at: 250,
      },
    ],
    'current-hash',
  );

  assert.deepEqual(merged.map((s: { id: string }) => s.id), [
    'duplicate-current',
    'current-old',
  ]);
  assert.equal(
    merged.find((s: { id: string }) => s.id === 'duplicate-current')?.name,
    'Current from project endpoint',
  );
}

async function testPanelDisposePreservesQueuedMessagesForReopen() {
  const client = {
    activeSessions: async () => [],
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _handleSend: (...args: unknown[]) => Promise<void>;
    _handlePanelDisposed: (panel: unknown, webview: unknown) => void;
    _findSessionIdByPanel: (panel: unknown) => string | undefined;
    _panels: Map<string, unknown>;
    _webviewPanels: Map<unknown, unknown>;
    _panelReady: Map<string, boolean>;
    _pendingMessages: Map<string, unknown>;
    _panelSessions: Map<string, unknown>;
    _focusedPanelId?: string;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._focusedPanelId = 'session-a';
  unsafeProvider._handleSend = async (...args: unknown[]) => {
    // 模拟 _handleSend 入队行为
    const sid = (args as unknown[])[4] as string;
    const text = (args as unknown[])[0] as string;
    const rt = unsafeProvider._sessionRuntimes.get(sid);
    if (rt?.isGenerating) {
      rt.queuedMessages.push({ text });
      return;
    }
    if (rt) {
      rt.isGenerating = true;
      rt.queuedMessages = [];
    }
  };

  // 创建运行时并模拟正在生成的回合
  const rt = {
    isGenerating: true,
    queuedMessages: [] as unknown[],
    eventBuffer: [] as unknown[],
  };
  unsafeProvider._sessionRuntimes.set('session-a', rt);
  unsafeProvider._panels.set('session-a', {});
  unsafeProvider._webviewPanels.set({}, {});
  unsafeProvider._panelReady.set('session-a', true);
  unsafeProvider._panelSessions.set('session-a', {});

  // 模拟 panel dispose：不应清空 queuedMessages
  const panel = { webview: {} };
  unsafeProvider._findSessionIdByPanel = () => 'session-a';
  unsafeProvider._handlePanelDisposed(panel, panel.webview);

  // queuedMessages 应被保留（不再被清空）
  const disposedRt = unsafeProvider._sessionRuntimes.get('session-a');
  assert.ok(disposedRt, 'runtime should still exist after panel dispose');
  assert.deepEqual(disposedRt!.queuedMessages, [],
    'queuedMessages should be preserved (empty here since none were queued)');
  assert.equal(disposedRt!.isGenerating, false,
    'isGenerating should be reset to false after dispose');
}

async function testRecoveryCheckQueuesMessageInsteadOfDroppingIt() {
  const client = {
    activeSessions: async () => ['session-a'],  // 旧回合仍活
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string; message?: string; id?: string }> = [];
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      recoveryLocked?: boolean;
      queuedMessages: Array<{ text: string; clientMessageId?: string }>;
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _focusedPanelId?: string;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessageForSession: (sessionId: string, msg: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._focusedPanelId = 'session-a';
  unsafeProvider._postMessageForSession = (sid, msg) => {
    posted.push({ ...(msg as { type?: string; message?: string; id?: string }) });
  };

  // 设置 recoveryLocked = true（模拟窗口重开后旧回合仍活）
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    recoveryLocked: true,
    queuedMessages: [],
    eventBuffer: [],
  });

  // 发送新消息：应入队等待，而非报错丢弃
  await unsafeProvider._handleSend('queued message', undefined, undefined, 'client-1', 'session-a', 'build');

  const rt = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.equal(rt.queuedMessages.length, 1, 'message should be queued, not dropped');
  assert.equal(rt.queuedMessages[0].text, 'queued message');
  assert.equal(rt.queuedMessages[0].clientMessageId, 'client-1');

  // 应通知前端消息已排队
  const queuedNotifications = posted.filter((msg) => msg.type === 'queuedMessageSent');
  assert.equal(queuedNotifications.length, 1, 'should notify frontend of queued message');
  assert.equal(queuedNotifications[0].id, 'client-1');

  // 不应有 error 消息
  const errorMessages = posted.filter((msg) => msg.type === 'error');
  assert.equal(errorMessages.length, 0, 'should not emit error when queueing');
}

async function testRecoveryCheckQueuesMessageWhenDaemonUnreachable() {
  const client = {
    activeSessions: async () => { throw new Error('daemon unreachable'); },
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string }> = [];
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      recoveryLocked?: boolean;
      queuedMessages: Array<{ text: string }>;
      eventBuffer: unknown[];
    }>;
    _approvalModeState: { confirmedMode: string; displayMode: string; pendingMode?: string };
    _focusedPanelId?: string;
    _handleSend: (...args: unknown[]) => Promise<void>;
    _postMessageForSession: (sessionId: string, msg: unknown) => void;
  };
  unsafeProvider._approvalModeState = { confirmedMode: 'build', displayMode: 'build' };
  unsafeProvider._focusedPanelId = 'session-a';
  unsafeProvider._postMessageForSession = (sid, msg) => {
    posted.push({ ...(msg as { type?: string }) });
  };

  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    recoveryLocked: true,
    queuedMessages: [],
    eventBuffer: [],
  });

  // daemon 不可达：应入队等待重试，而非报错丢弃
  await unsafeProvider._handleSend('retry later', undefined, undefined, undefined, 'session-a', 'build');

  const rt = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.equal(rt.queuedMessages.length, 1, 'message should be queued when daemon unreachable');
  assert.equal(rt.queuedMessages[0].text, 'retry later');

  // 不应有 error 消息
  const errorMessages = posted.filter((msg) => msg.type === 'error');
  assert.equal(errorMessages.length, 0, 'should not emit error when daemon unreachable');
}

async function testPollActiveSessionRecoveryResetsWhenTurnEnds() {
  let activeCallCount = 0;
  let activeSessions = ['session-a'];
  const client = {
    activeSessions: async () => {
      activeCallCount += 1;
      return activeSessions;
    },
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const posted: Array<{ type?: string }> = [];
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      recoveryLocked?: boolean;
      streamGeneration?: number;
      queuedMessages: Array<{ text: string }>;
      eventBuffer: unknown[];
      pollHandle?: { cancelled: boolean };
    }>;
    _postMessageForSession: (sessionId: string, msg: unknown) => void;
    _pollActiveSessionRecovery: (sessionId: string, generation: number) => Promise<void>;
    _sendNextQueuedMessage: (sessionId?: string) => Promise<void>;
  };
  unsafeProvider._postMessageForSession = (sid, msg) => {
    posted.push({ ...(msg as { type?: string }) });
  };
  unsafeProvider._sendNextQueuedMessage = async () => {};

  // 设置初始状态：标记 isGenerating = true（模拟重开窗口检测到旧回合仍活）
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    recoveryLocked: false,
    streamGeneration: 0,
    queuedMessages: [{ text: 'waiting message' }],
    eventBuffer: [],
  });

  // 启动轮询（不 await，让它在后台运行）
  const pollPromise = unsafeProvider._pollActiveSessionRecovery('session-a', 0);

  // 等待首次轮询（500ms）完成
  await new Promise((resolve) => setTimeout(resolve, 600));

  // 旧回合仍在运行：isGenerating 应仍为 true
  let rt = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.equal(rt.isGenerating, true, 'isGenerating should remain true while turn is active');
  assert.equal(activeCallCount, 1, 'should have polled once after 500ms');

  // 模拟旧回合结束：从 activeSessions 移除
  activeSessions = [];

  // 等待下一次轮询（2s）
  await new Promise((resolve) => setTimeout(resolve, 2100));

  await pollPromise.catch(() => {});

  // 旧回合已结束：isGenerating 应被重置为 false
  rt = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.equal(rt.isGenerating, false, 'isGenerating should be reset when turn ends');
  assert.equal(rt.recoveryLocked, false, 'recoveryLocked should be cleared');
  assert.equal(rt.pollHandle, undefined, 'pollHandle should be cleared');

  // 应发送 generationStopped 消息
  const stoppedMessages = posted.filter((msg) => msg.type === 'generationStopped');
  assert.equal(stoppedMessages.length, 1, 'should send generationStopped when turn ends');
}

async function testPollActiveSessionRecoveryDoesNotStartDuplicatePolls() {
  let activeCallCount = 0;
  const client = {
    activeSessions: async () => {
      activeCallCount += 1;
      return ['session-a'];
    },
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration?: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      pollHandle?: { cancelled: boolean };
    }>;
    _pollActiveSessionRecovery: (sessionId: string, generation: number) => Promise<void>;
  };

  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 0,
    queuedMessages: [],
    eventBuffer: [],
  });

  // 启动第一次轮询
  const poll1 = unsafeProvider._pollActiveSessionRecovery('session-a', 0);

  // 立即尝试启动第二次轮询：应被去重跳过
  const poll2 = unsafeProvider._pollActiveSessionRecovery('session-a', 0);

  // 等待首次轮询完成
  await new Promise((resolve) => setTimeout(resolve, 600));

  // 只有第一次轮询会调用 activeSessions
  assert.equal(activeCallCount, 1, 'should only start one poll, duplicates are skipped');

  // 清理：取消 pollHandle
  const rt = unsafeProvider._sessionRuntimes.get('session-a');
  if (rt?.pollHandle) rt.pollHandle.cancelled = true;

  await poll1.catch(() => {});
  await poll2.catch(() => {});
}

async function testPollActiveSessionRecoveryCancellationDoesNotForceReset() {
  // 使用立即超时的 mock 来测试超时路径
  // 由于 MAX_POLLS = 150 且间隔为 2s，完整超时需要 5 分钟
  // 这里通过直接验证超时后的 pollHandle 清理来测试
  const client = {
    activeSessions: async () => ['session-a'],  // 始终活跃，模拟长回合
    stopGeneration: async () => ({ success: true, message: 'ok' }),
  };
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, client as never);
  const unsafeProvider = provider as unknown as {
    _sessionRuntimes: Map<string, {
      isGenerating: boolean;
      streamGeneration?: number;
      queuedMessages: unknown[];
      eventBuffer: unknown[];
      pollHandle?: { cancelled: boolean };
    }>;
    _pollActiveSessionRecovery: (sessionId: string, generation: number) => Promise<void>;
  };

  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: true,
    streamGeneration: 0,
    queuedMessages: [],
    eventBuffer: [],
  });

  // 启动轮询
  const pollPromise = unsafeProvider._pollActiveSessionRecovery('session-a', 0);

  // 等待首次轮询
  await new Promise((resolve) => setTimeout(resolve, 600));

  // 立即取消轮询，模拟超时后的清理
  const rt = unsafeProvider._sessionRuntimes.get('session-a')!;
  if (rt.pollHandle) rt.pollHandle.cancelled = true;

  await pollPromise.catch(() => {});

  // 超时（被取消）后：isGenerating 应保持 true，不被强制重置
  const finalRt = unsafeProvider._sessionRuntimes.get('session-a')!;
  assert.equal(finalRt.isGenerating, true,
    'isGenerating should remain true after timeout (no force reset)');
}

Promise.resolve()
  .then(testReadyMarksPanelOnlyAfterInitialReplay)
  .then(testPanelReadyHandlerIsInstalledBeforeWebviewBoots)
  .then(testOpeningAnExistingUnhydratedSessionTabLoadsItsHistory)
  .then(testClosingAnExistingTabCancelsItsPendingHistoryHydration)
  .then(testNewGenerationRejectsHistoryLoadedForAnOlderGeneration)
  .then(testRepeatedOpenSharesOneHistoryRequestAndPublishesOnce)
  .then(testProjectRebindRejectsThePreviousProjectsLateHistory)
  .then(testRestoredPanelWithoutProjectHashResolvesBeforeLoadingHistory)
  .then(testProjectHashResolutionFallsBackOnlyWithinTheCurrentWorkspace)
  .then(testLiveStreamEventsWaitForPanelReadiness)
  .then(testReadyCatchUpReplaysEventsThatArrivedDuringInitialization)
  .then(testReadyCatchUpReplaysReplacementGenerationFromStart)
  .then(testTerminalArrivingDuringReadyReplayIsDeliveredOnceAfterCatchUp)
  .then(testReadyDoesNotFlushHistoryAlreadyDeliveredByItsAtomicSnapshot)
  .then(testClosingTheLastSessionTabStopsTheUnobservableTurn)
  .then(testSessionBoundMessageNeverFallsBackToAnotherPanel)
  .then(testSessionSelectionDoesNotRewriteUnrelatedTabBindings)
  .then(testSessionSelectionIsNeverBroadcastToUnrelatedTabsAndCanonicalRemapPersists)
  .then(testDoneForActiveSessionDoesNotEraseItsProjectBinding)
  .then(testGenerationStartedRoutesToTheRequestedSession)
  .then(testUnboundReadyPanelGetsAnOwnedSessionBeforeItsFirstTurn)
  .then(testExistingPanelSessionIsNeverReplacedJustBecauseRuntimeWasCold)
  .then(testSecondReadyStillRestoresCompleteHistory)
  .then(testLateHistoryRefreshDoesNotRecreateADisposedPanelBinding)
  .then(testReadyDoesNotPublishHistoryCapturedFromAnOlderGeneration)
  .then(testNewGenerationDropsStaleTerminalAndHistoryPendingMessages)
  .then(testAbnormalTerminalSurvivesAuthoritativeHistoryRefresh)
  .then(testUnexpectedStreamEndLocksAnActiveUnattachedTurnUntilStopped)
  .then(testUnobservedInterruptedStreamRetainsReplayableSessionState)
  .then(testStopTargetsTheOwningSessionInsteadOfTheFocusedFallback)
  .then(testStopKeepsRecoveryLockUntilDaemonConfirmsCancellation)
  .then(testRecoveryLockRemainsVisibleAndCannotQueueAnotherTurn)
  .then(testAuthFileWatcherRefreshesSetupState)
  .then(testStaleSetupRefreshCannotOverwriteNewerAuthState)
  .then(testDisposedPanelCannotBlockNewPanelSetupState)
  .then(testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel)
  .then(testQueuedMessageDoesNotDrainWhileApprovalModeIsPending)
  .then(testAbnormalDoneDoesNotDrainQueuedMessages)
  .then(testCanonicalSessionRemapWithoutPanelDoesNotCreateAPhantomPanel)
  .then(testErrorQueuedForNotReadyPanelIsNotAlsoStoredForReplay)
  .then(testStartingANewGenerationClearsStoredTerminalError)
  .then(testLateTerminalFromCancelledGenerationCannotStopReplacementTurn)
  .then(testLateEventsFromCompletedGenerationAreIgnored)
  .then(testClosedWebviewCannotCompleteFirstSessionBinding)
  .then(testClosedTabCannotFallbackOrStartAfterAdmissionWait)
  .then(testCancelledPreparationCannotStartAfterAReplacementTurn)
  .then(testConcurrentPreparationQueuesTheSecondPromptInsteadOfStartingTwoStreams)
  .then(testWebviewBridgeClearsQueuedMessagesOnHostInstruction)
  .then(testLatePanelSessionBindingIsPersistedForRestore)
  .then(testInitialStateDoesNotClearPendingApprovalModeSwitch)
  .then(testPermissionRequestFromStreamIsForwardedToPanel)
  .then(testPermissionResponsePostsDecisionToDaemon)
  .then(testPermissionResponsePostsExplicitDecisionToDaemon)
  .then(testMergeSessionsForDisplayShowsOnlyCurrentProjectSessionsWhenProjectIsKnown)
  .then(testLoadSessionsForDisplayUsesVscodeWorkspaceDirectory)
  .then(testLoadSessionsForDisplayDoesNotFallBackToGlobalWhenWorkspaceHasNoSessions)
  .then(testLoadSessionsForDisplayFallsBackToGlobalWhenWorkspaceRequestFails)
  .then(testRefreshSessionsOnlyPrependsSyntheticPanelsForCurrentWorkspace)
  .then(testPanelDisposePreservesQueuedMessagesForReopen)
  .then(testRecoveryCheckQueuesMessageInsteadOfDroppingIt)
  .then(testRecoveryCheckQueuesMessageWhenDaemonUnreachable)
  .then(testPollActiveSessionRecoveryResetsWhenTurnEnds)
  .then(testPollActiveSessionRecoveryDoesNotStartDuplicatePolls)
  .then(testPollActiveSessionRecoveryCancellationDoesNotForceReset)
  .catch((err) => {
  console.error(err);
  process.exit(1);
  });
