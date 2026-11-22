import assert from 'node:assert/strict';
import * as http from 'node:http';
import { once } from 'node:events';
import { test } from 'node:test';
import { DaemonClient } from '../../src/daemon/client';

test('cancelling an inline completion closes its daemon POST', { timeout: 2_000 }, async () => {
  const server = http.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address !== 'string');

  try {
    const client = new DaemonClient(address.port);
    const controller = new AbortController();
    const received = once(server, 'request');
    const request = client.requestCueCompletion({
      file_path: 'example.ts',
      language_id: 'typescript',
      prefix: 'const n = +',
      suffix: ';',
    }, controller.signal);
    const cancelled = assert.rejects(request, { name: 'AbortError' });
    const [incoming, response] = await received;
    assert.equal(incoming.url, '/cue/complete');
    const closed = once(response, 'close');
    controller.abort();
    await Promise.all([cancelled, closed]);
    assert.equal(response.writableEnded, false);
  } finally {
    server.closeAllConnections();
    server.close();
  }
});
