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
    return new Response(
      '{"accepted":true,"disposition":"steered","generation":3,"turn_id":7}',
      { status: 200 },
    );
  }) as typeof fetch;

  try {
    const { postLiveMessage } = await import('./api.ts');

    const receipt = await postLiveMessage('hello', undefined, undefined, 'session-1', 'input-1');

    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, '/live/message');
    const body = JSON.parse(String(calls[0].init?.body));
    assert.deepEqual(body, {
      message: 'hello',
      session_id: 'session-1',
      client_input_id: 'input-1',
    });
    assert.deepEqual(receipt, { disposition: 'steered', generation: 3, turn_id: 7 });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postLiveMessage accepts the legacy accepted-only receipt without retrying', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response('{"accepted":true}', { status: 200 })) as typeof fetch;
  try {
    const { postLiveMessage } = await import('./api.ts');
    assert.deepEqual(await postLiveMessage('hello'), {
      disposition: 'started',
      generation: 0,
      turn_id: 0,
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postLiveMessage exposes the provider actually used for a busy-turn steer', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(JSON.stringify({
    accepted: true,
    disposition: 'steered',
    generation: 4,
    turn_id: 9,
    provider: 'provider-active',
    provider_change_applied: false,
  }), { status: 200 })) as typeof fetch;
  try {
    const { postLiveMessage } = await import('./api.ts');
    assert.deepEqual(await postLiveMessage('continue', undefined, 'provider-requested'), {
      disposition: 'steered',
      generation: 4,
      turn_id: 9,
      provider: 'provider-active',
      providerChangeApplied: false,
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

test('live session switch returns a structured active-turn rejection', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify({ ok: false, active_turn: true, error: 'runtime is busy' }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { postLiveSwitchSession } = await import('./api.ts');
    assert.deepEqual(await postLiveSwitchSession('session-2'), {
      ok: false,
      activeTurn: true,
      error: 'runtime is busy',
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('live mode still rejects protocol-level failures', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify({ ok: false, error: 'runtime is busy' }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { postLiveMode } = await import('./api.ts');
    await assert.rejects(
      () => postLiveMode('plan'),
      /rejected the mode switch/,
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postLiveUserInput rejects an answer the runtime did not accept', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(
    JSON.stringify({ accepted: false }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )) as typeof fetch;

  try {
    const { postLiveUserInput } = await import('./api.ts');
    await assert.rejects(
      () => postLiveUserInput({
        request_id: 42,
        declined: false,
        selected: ['继续'],
        text: null,
      }),
      /did not accept/i,
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postLivePolicyInterventionResolution correlates the typed recovery action', async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), init });
    return new Response('{"accepted":true}', { status: 200 });
  }) as typeof fetch;

  try {
    const { postLivePolicyInterventionResolution } = await import('./api.ts');
    await postLivePolicyInterventionResolution(42, 'skip_step');
    assert.equal(calls[0].url, '/live/policy-intervention');
    assert.deepEqual(JSON.parse(String(calls[0].init?.body)), {
      intervention_id: 42,
      action: 'skip_step',
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('postChatUserInput correlates the answer by session and native request id', async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), init });
    return new Response('{"accepted":true}', { status: 200 });
  }) as typeof fetch;

  try {
    const { postChatUserInput } = await import('./api.ts');
    await postChatUserInput('session-1', {
      request_id: 42,
      declined: false,
      selected: ['继续'],
      text: null,
    });

    assert.equal(calls[0].url, '/chat/user-input');
    assert.deepEqual(JSON.parse(String(calls[0].init?.body)), {
      session_id: 'session-1',
      request_id: 42,
      declined: false,
      selected: ['继续'],
      text: null,
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('a one-question questions payload still uses the batch response protocol', async () => {
  const { isUserInputBatch } = await import('./api.ts');
  assert.equal(isUserInputBatch({
    type: 'user_input_request',
    request_id: 42,
    header: '',
    question: '',
    mode: 'single',
    options: [],
    questions: [{
      header: 'Pick',
      question: 'Red or blue?',
      mode: 'single',
      options: [{ label: 'Red' }, { label: 'Blue' }],
    }],
  }), true);
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
