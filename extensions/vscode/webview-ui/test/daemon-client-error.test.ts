import assert from 'node:assert/strict';
import * as http from 'node:http';
import { DaemonClient, classifyDaemonStreamError, formatDaemonHttpError } from '../../src/daemon/client';

function testBodyLimitErrorIsReadable() {
  assert.equal(
    formatDaemonHttpError(413, 'Failed to buffer the request body: length limit exceeded'),
    '消息内容过大，发送失败。请压缩图片、减少附件数量，或缩短消息后重试。',
  );
}

function testJsonWrappedBodyLimitErrorIsReadable() {
  assert.equal(
    formatDaemonHttpError(413, JSON.stringify({ message: 'Failed to buffer the request body: length limit exceeded' })),
    '消息内容过大，发送失败。请压缩图片、减少附件数量，或缩短消息后重试。',
  );
}

function testRegularErrorsKeepServerMessage() {
  assert.equal(formatDaemonHttpError(500, JSON.stringify({ error: 'boom' })), 'boom');
}

function testManualAbortStreamErrorIsStopped() {
  assert.deepEqual(classifyDaemonStreamError('aborted', true), { type: 'stopped' });
}

function testNonManualAbortStreamErrorKeepsMessage() {
  assert.deepEqual(classifyDaemonStreamError('socket hang up', false), {
    type: 'error',
    message: 'Stream error: socket hang up',
  });
}

function testPermissionRequestSseIsForwardedToCallback() {
  const client = new DaemonClient(13456);
  let received: unknown;
  const callbacks = {
    onText: () => undefined,
    onToolBatch: () => undefined,
    onToolStart: () => undefined,
    onToolResult: () => undefined,
    onTokens: () => undefined,
    onArtifactStart: () => undefined,
    onArtifactContent: () => undefined,
    onArtifactEnd: () => undefined,
    onDone: () => undefined,
    onStopped: () => undefined,
    onError: () => undefined,
    onWarning: () => undefined,
    onRateLimited: () => undefined,
    onPermissionRequest: (request: unknown) => {
      received = request;
    },
  };

  (client as unknown as { handleSSEData: (data: string, callbacks: unknown) => void })
    .handleSSEData(JSON.stringify({
      type: 'permission_request',
      session_id: 'session-1',
      tool_name: 'write_file',
      reason: 'Modify workspace file',
      call_id: 'call-1',
      arguments: '{"path":"README.md"}',
    }), callbacks);

  assert.deepEqual(received, {
    sessionId: 'session-1',
    toolName: 'write_file',
    reason: 'Modify workspace file',
    callId: 'call-1',
    args: '{"path":"README.md"}',
  });
}

function testWarningSseIsForwardedToCallback() {
  const client = new DaemonClient(13456);
  let received: unknown;
  const callbacks = {
    onText: () => undefined,
    onToolBatch: () => undefined,
    onToolStart: () => undefined,
    onToolResult: () => undefined,
    onTokens: () => undefined,
    onArtifactStart: () => undefined,
    onArtifactContent: () => undefined,
    onArtifactEnd: () => undefined,
    onDone: () => undefined,
    onStopped: () => undefined,
    onError: () => undefined,
    onWarning: (message: string) => {
      received = message;
    },
    onRateLimited: () => undefined,
    onPermissionRequest: () => undefined,
  };

  (client as unknown as { handleSSEData: (data: string, callbacks: unknown) => void })
    .handleSSEData(JSON.stringify({
      type: 'warning',
      message: 'temporary degraded stream',
    }), callbacks);

  assert.equal(received, 'temporary degraded stream');
}

function testRateLimitedSseIsForwardedToCallback() {
  const client = new DaemonClient(13456);
  let received: unknown;
  const callbacks = {
    onText: () => undefined,
    onToolBatch: () => undefined,
    onToolStart: () => undefined,
    onToolResult: () => undefined,
    onTokens: () => undefined,
    onArtifactStart: () => undefined,
    onArtifactContent: () => undefined,
    onArtifactEnd: () => undefined,
    onDone: () => undefined,
    onStopped: () => undefined,
    onError: () => undefined,
    onWarning: () => undefined,
    onRateLimited: (event: unknown) => {
      received = event;
    },
    onPermissionRequest: () => undefined,
  };

  (client as unknown as { handleSSEData: (data: string, callbacks: unknown) => void })
    .handleSSEData(JSON.stringify({
      type: 'rate_limited',
      message: 'Rate limit reached',
      retry_after_seconds: 2,
      attempt: 3,
      max_attempts: 5,
    }), callbacks);

  assert.deepEqual(received, {
    message: 'Rate limit reached',
    retryAfterSeconds: 2,
    attempt: 3,
    maxAttempts: 5,
  });
}

function testDoneSseForwardsAuthoritativeStopReason() {
  const client = new DaemonClient(13456);
  let received: unknown;
  const callbacks = {
    onText: () => undefined,
    onToolBatch: () => undefined,
    onToolStart: () => undefined,
    onToolProgress: () => undefined,
    onToolResult: () => undefined,
    onTokens: () => undefined,
    onArtifactStart: () => undefined,
    onArtifactContent: () => undefined,
    onArtifactEnd: () => undefined,
    onDone: (...args: unknown[]) => { received = args; },
    onStopped: () => undefined,
    onError: () => undefined,
    onWarning: () => undefined,
    onRateLimited: () => undefined,
    onPermissionRequest: () => undefined,
  };

  (client as unknown as { handleSSEData: (data: string, callbacks: unknown) => void })
    .handleSSEData(JSON.stringify({
      type: 'done',
      tokens: 42,
      tool_calls: 4,
      session_id: 'session-1',
      stop_reason: 'tool_loop_detected',
      message: 'The turn stopped before another repeated call.',
    }), callbacks);

  assert.deepEqual(received, [
    42,
    4,
    'session-1',
    'tool_loop_detected',
    'The turn stopped before another repeated call.',
  ]);
}

async function testCleanEofWithoutTerminalFailsTheStream() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    response.end('data: {"type":"text","content":"partial"}\n\n');
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));

  try {
    const address = server.address();
    assert.ok(address && typeof address !== 'string');
    const client = new DaemonClient(address.port);
    const errors: string[] = [];
    const terminal = new Promise<void>((resolve) => {
      client.streamChat({ message: 'hello' }, {
        onText: () => undefined,
        onToolBatch: () => undefined,
        onToolStart: () => undefined,
        onToolProgress: () => undefined,
        onToolResult: () => undefined,
        onTokens: () => undefined,
        onArtifactStart: () => undefined,
        onArtifactContent: () => undefined,
        onArtifactEnd: () => undefined,
        onDone: () => resolve(),
        onStopped: () => resolve(),
        onError: (message) => {
          errors.push(message);
          resolve();
        },
        onWarning: () => undefined,
        onRateLimited: () => undefined,
        onPermissionRequest: () => undefined,
      });
    });

    await Promise.race([
      terminal,
      new Promise((_, reject) => setTimeout(() => reject(new Error('stream EOF produced no terminal callback')), 1_000)),
    ]);
    assert.deepEqual(errors, ['Stream ended before a terminal event']);
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
}

async function testCleanEofAfterDoneDoesNotEmitASecondTerminal() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    response.end('data: {"type":"done","tokens":1,"tool_calls":0,"stop_reason":"stopped"}\n\n');
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));

  try {
    const address = server.address();
    assert.ok(address && typeof address !== 'string');
    const client = new DaemonClient(address.port);
    const terminals: string[] = [];
    client.streamChat({ message: 'hello' }, {
      onText: () => undefined,
      onToolBatch: () => undefined,
      onToolStart: () => undefined,
      onToolProgress: () => undefined,
      onToolResult: () => undefined,
      onTokens: () => undefined,
      onArtifactStart: () => undefined,
      onArtifactContent: () => undefined,
      onArtifactEnd: () => undefined,
      onDone: () => { terminals.push('done'); },
      onStopped: () => { terminals.push('stopped'); },
      onError: () => { terminals.push('error'); },
      onWarning: () => undefined,
      onRateLimited: () => undefined,
      onPermissionRequest: () => undefined,
    });

    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(terminals, ['done']);
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
}

async function testEventsAfterTerminalAreIgnored() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    response.end([
      'data: {"type":"done","tokens":1,"tool_calls":0,"stop_reason":"stopped"}',
      'data: {"type":"text","content":"late text"}',
      '',
    ].join('\n'));
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));

  try {
    const address = server.address();
    assert.ok(address && typeof address !== 'string');
    const client = new DaemonClient(address.port);
    const terminals: string[] = [];
    const text: string[] = [];
    client.streamChat({ message: 'hello' }, {
      onText: (content) => { text.push(content); },
      onToolBatch: () => undefined,
      onToolStart: () => undefined,
      onToolProgress: () => undefined,
      onToolResult: () => undefined,
      onTokens: () => undefined,
      onArtifactStart: () => undefined,
      onArtifactContent: () => undefined,
      onArtifactEnd: () => undefined,
      onDone: () => { terminals.push('done'); },
      onStopped: () => { terminals.push('stopped'); },
      onError: () => { terminals.push('error'); },
      onWarning: () => undefined,
      onRateLimited: () => undefined,
      onPermissionRequest: () => undefined,
    });

    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(terminals, ['done']);
    assert.deepEqual(text, []);
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
}

testBodyLimitErrorIsReadable();
testJsonWrappedBodyLimitErrorIsReadable();
testRegularErrorsKeepServerMessage();
testManualAbortStreamErrorIsStopped();
testNonManualAbortStreamErrorKeepsMessage();
testPermissionRequestSseIsForwardedToCallback();
testWarningSseIsForwardedToCallback();
testRateLimitedSseIsForwardedToCallback();
testDoneSseForwardsAuthoritativeStopReason();
void Promise.resolve()
  .then(testCleanEofWithoutTerminalFailsTheStream)
  .then(testCleanEofAfterDoneDoesNotEmitASecondTerminal)
  .then(testEventsAfterTerminalAreIgnored)
  .catch((error) => {
    console.error(error);
    process.exit(1);
  });
