import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createApiFetch, loadRelayCredentials, relayRequest } from './transport.ts';
const credentials = { route: 'route-01234567890123456789', token: 'webui-01234567890123456789' };

test('relay HTTP and SSE preserve body/signal and separate both credentials', () => {
  const signal = new AbortController().signal;
  for (const path of ['/live?session_id=a', '/chat', '/auth/status']) {
    const [url, init] = relayRequest(credentials, path, { method: 'POST', body: 'payload', signal, headers: { 'X-AtomCode-Client': 'webui' } });
    assert.equal(url, '/api' + path);
    const headers = new Headers(init.headers);
    assert.equal(headers.get('X-Atom-Token'), credentials.route);
    assert.equal(headers.get('Authorization'), 'Bearer ' + credentials.token);
    assert.equal(headers.get('X-AtomCode-Client'), 'webui');
    assert.equal(init.body, 'payload');
    assert.equal(init.signal, signal);
    assert.equal(init.credentials, 'omit');
    assert.equal(init.redirect, 'error');
  }
});
test('credentials never go to an external URL or protocol-relative path', () => {
  for (const path of ['https://evil.example', '//evil.example', '/\\evil.example', 'data:text/plain,foo']) {
    assert.throws(() => relayRequest(credentials, path), /local API path/);
  }
});
test('pairing survives a tab reload; invalid replacement cannot reuse the previous machine', () => {
  const saved = new Map<string, string>();
  const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value); }, removeItem: (key: string) => { saved.delete(key); } };
  assert.deepEqual(loadRelayCredentials(`#relay=${credentials.route}&token=${credentials.token}`, storage), credentials);
  assert.deepEqual(loadRelayCredentials('', storage), credentials);
  assert.equal(loadRelayCredentials('#relay=invalid&token=invalid', storage), null);
  assert.equal(loadRelayCredentials('', storage), null);
  assert.deepEqual(loadRelayCredentials(`#relay=${credentials.route}&token=${credentials.token}`), credentials);
});

test('an unpaired relay page asks to pair again without contacting local endpoints', async () => {
  let calls = 0;
  const fetchFn: typeof fetch = async () => { calls++; return new Response('{}'); };
  const remote = createApiFetch(true, null, fetchFn);
  assert.equal((await remote('/project')).status, 401);
  assert.equal((await remote('/auth/status')).status, 401);
  assert.equal(calls, 0);
  const local = createApiFetch(false, null, fetchFn);
  assert.equal((await local('/project')).status, 200);
  assert.equal(calls, 1);
});
