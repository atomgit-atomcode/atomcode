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
  };

  const sent: unknown[][] = [];
  unsafeProvider._focusedPanelId = undefined;
  unsafeProvider._sessionRuntimes.set('session-a', {
    isGenerating: false,
    queuedMessages: [{ text: 'next prompt', context: [{ path: 'file.ts', type: 'file' }], clientMessageId: 'queued-1' }],
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
  ]);
}

testQueuedMessageDrainsForCompletedSessionWithoutFocusedPanel().catch((err) => {
  console.error(err);
  process.exit(1);
});
