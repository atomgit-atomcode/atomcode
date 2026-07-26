import { test } from 'node:test';
import assert from 'node:assert/strict';

Object.defineProperty(globalThis, 'location', {
  value: new URL('http://localhost/?token=test-token'),
  configurable: true,
});

test('postLiveMessage does not send approval_mode because live mode is global', async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), init });
    return new Response('{"accepted":true}', { status: 200 });
  }) as typeof fetch;

  try {
    const { postLiveMessage } = await import('./api.ts');

    await postLiveMessage('hello', undefined, undefined, 'session-1', 'plan');

    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, '/live/message');
    const body = JSON.parse(String(calls[0].init?.body));
    assert.deepEqual(body, {
      message: 'hello',
      session_id: 'session-1',
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postLiveProvider scopes the runtime switch to the active session', async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), init });
    return new Response('{"ok":true}', { status: 200 });
  }) as typeof fetch;

  try {
    const { postLiveProvider } = await import('./api.ts');

    await postLiveProvider('provider-b', 'session-1');

    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, '/live/provider');
    assert.deepEqual(JSON.parse(String(calls[0].init?.body)), {
      provider: 'provider-b',
      session_id: 'session-1',
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('live control APIs reject protocol-level failures', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify({ ok: false, error: 'runtime is busy' }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { postLiveMode, postLiveSwitchSession } = await import('./api.ts');
    await assert.rejects(
      () => postLiveSwitchSession('session-2'),
      /runtime is busy/,
    );
    await assert.rejects(
      () => postLiveMode('plan'),
      /rejected the mode switch/,
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('collection APIs reject server error payloads instead of returning non-arrays', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify('session metadata is missing'),
    { status: 500, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { getModels, getProjects } = await import('./api.ts');
    await assert.rejects(() => getModels(), /list models failed: 500/);
    await assert.rejects(() => getProjects(), /list projects failed: 500/);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('deleteSession surfaces the daemon conflict reason', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify({
      success: false,
      error: 'This session is active. Switch to or create another session, then try again.',
      code: 'SESSION_IN_USE',
      retryable: false,
    }),
    { status: 409, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { deleteSession, DeleteSessionError } = await import('./api.ts');
    const error = await deleteSession('0123456789abcdef', 's1').catch((cause) => cause);
    assert.ok(error instanceof DeleteSessionError);
    assert.equal(error.code, 'SESSION_IN_USE');
    assert.match(error.message, /session is active/i);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('streamChat rejects a clean EOF without an authoritative terminal', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    'data: {"type":"text","content":"partial"}\n\n',
    { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
  )) as typeof fetch;

  try {
    const { streamChat } = await import('./api.ts');
    const events: unknown[] = [];
    await assert.rejects(
      () => streamChat({ message: 'hello' }, (event) => events.push(event)),
      /ended before an authoritative terminal/i,
    );
    assert.deepEqual(events, [{ type: 'text', content: 'partial' }]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('streamChat accepts done, stopped, and error as authoritative terminals', async () => {
  const originalFetch = globalThis.fetch;
  const { streamChat } = await import('./api.ts');

  try {
    for (const terminal of [
      { type: 'done', tokens: null, tool_calls: null, session_id: 'session-1' },
      { type: 'stopped' },
      { type: 'error', message: 'provider failed' },
    ]) {
      globalThis.fetch = (async () => new Response(
        `data: ${JSON.stringify(terminal)}\n\n`,
        { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
      )) as typeof fetch;
      await streamChat({ message: 'hello' }, () => {});
    }
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('cancelDetachedChat aborts the local stream and uses the existing stop protocol', async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), init });
    return new Response(null, { status: 200 });
  }) as typeof fetch;

  try {
    const { cancelDetachedChat } = await import('./api.ts');
    const controller = new AbortController();
    await cancelDetachedChat('request-1', controller);

    assert.equal(controller.signal.aborted, true);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, '/chat/stop');
    assert.deepEqual(JSON.parse(String(calls[0].init?.body)), {
      session_id: 'request-1',
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('getActiveChatSessions reads the authoritative detached chat registry', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL) => {
    assert.equal(String(url), '/chat/active');
    return new Response(JSON.stringify(['session-1', 'session-2']), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  }) as typeof fetch;

  try {
    const { getActiveChatSessions } = await import('./api.ts');
    assert.deepEqual(await getActiveChatSessions(), ['session-1', 'session-2']);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
