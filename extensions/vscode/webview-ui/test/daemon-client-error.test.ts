import assert from 'node:assert/strict';
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

testBodyLimitErrorIsReadable();
testJsonWrappedBodyLimitErrorIsReadable();
testRegularErrorsKeepServerMessage();
testManualAbortStreamErrorIsStopped();
testNonManualAbortStreamErrorKeepsMessage();
testPermissionRequestSseIsForwardedToCallback();
