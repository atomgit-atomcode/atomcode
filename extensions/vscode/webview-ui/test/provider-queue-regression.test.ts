import assert from 'node:assert/strict';
import Module from 'node:module';

declare const require: {
  (id: string): typeof import('../../src/chat/provider');
};

const originalLoad = (Module as unknown as { _load: typeof Module['_load'] })._load;
(Module as unknown as { _load: typeof Module['_load'] })._load = function patchedLoad(request, parent, isMain) {
  if (request === 'vscode') {
    return {
      Uri: { joinPath: (...parts: Array<{ fsPath?: string } | string>) => ({ fsPath: parts.map((p) => typeof p === 'string' ? p : p.fsPath || '').join('/') }) },
      workspace: { workspaceFolders: [] },
      window: {},
      env: { language: 'en' },
      commands: {},
      l10n: { t: (value: string) => value },
      ViewColumn: { Beside: 2, Active: -1 },
    };
  }
  return originalLoad.call(this, request, parent, isMain);
};

const { ChatViewProvider } = require('../../src/chat/provider');

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

Promise.resolve()
  .then(testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel)
  .then(testQueuedMessageDoesNotDrainWhileApprovalModeIsPending)
  .then(testInitialStateDoesNotClearPendingApprovalModeSwitch)
  .then(testPermissionRequestFromStreamIsForwardedToPanel)
  .then(testPermissionResponsePostsDecisionToDaemon)
  .then(testPermissionResponsePostsExplicitDecisionToDaemon)
  .catch((err) => {
  console.error(err);
  process.exit(1);
  });
