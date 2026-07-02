import assert from 'node:assert/strict';
import { classifyDaemonStreamError, formatDaemonHttpError } from '../../src/daemon/client';

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

testBodyLimitErrorIsReadable();
testJsonWrappedBodyLimitErrorIsReadable();
testRegularErrorsKeepServerMessage();
testManualAbortStreamErrorIsStopped();
testNonManualAbortStreamErrorKeepsMessage();
