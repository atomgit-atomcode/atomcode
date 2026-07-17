import assert from 'node:assert/strict';
import Module from 'node:module';

declare const require: {
  (id: string): typeof import('../../src/chat/provider');
};

const originalLoad = (Module as unknown as { _load: typeof Module['_load'] })._load;
const vscodeMock = {
  Uri: { joinPath: (...parts: Array<{ fsPath?: string } | string>) => ({ fsPath: parts.map((p) => typeof p === 'string' ? p : p.fsPath || '').join('/') }) },
  workspace: { workspaceFolders: [] as Array<{ uri: { fsPath: string } }> },
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

(Module as unknown as { _load: typeof Module['_load'] })._load = originalLoad;

async function testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel() {
  const provider = new ChatViewProvider({ fsPath: '/extension' } as never, {} as never);
  const unsafeProvider = provider as unknown as {
    _focusedPanelId?: string;
    _sessionRuntimes: Map<string, { isGenerating: boolean; queuedMessages: unknown[]; eventBuffer: unknown[] }>;
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
    _postMessageToPanel: (sessionId: string, msg: unknown) => void;
  };

  unsafeProvider._postMessage = () => undefined;
  unsafeProvider._postMessageToPanel = (_sessionId: string, msg: unknown) => {
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
      return [{ id: 'global', name: 'Global fallback', project_hash: 'fallback-hash', updated_at: 300 }];
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
  assert.deepEqual(loaded.sessions.map((s) => s.id), ['global']);

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

Promise.resolve()
  .then(testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel)
  .then(testQueuedMessageDoesNotDrainWhileApprovalModeIsPending)
  .then(testInitialStateDoesNotClearPendingApprovalModeSwitch)
  .then(testPermissionRequestFromStreamIsForwardedToPanel)
  .then(testPermissionResponsePostsDecisionToDaemon)
  .then(testPermissionResponsePostsExplicitDecisionToDaemon)
  .then(testMergeSessionsForDisplayShowsOnlyCurrentProjectSessionsWhenProjectIsKnown)
  .then(testLoadSessionsForDisplayUsesVscodeWorkspaceDirectory)
  .then(testLoadSessionsForDisplayDoesNotFallBackToGlobalWhenWorkspaceHasNoSessions)
  .then(testLoadSessionsForDisplayFallsBackToGlobalWhenWorkspaceRequestFails)
  .then(testRefreshSessionsOnlyPrependsSyntheticPanelsForCurrentWorkspace)
  .catch((err) => {
  console.error(err);
  process.exit(1);
  });
