import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createApiFetch, loadRelayCredentials, relayRequest, relayPairingSaved } from './transport.ts';
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


test('a pairing fragment is removed only after the exact pairing survives storage', () => {
  const saved = new Map<string, string>();
  const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value); }, removeItem: (key: string) => { saved.delete(key); } };
  const hash = `#relay=${credentials.route}&token=${credentials.token}`;
  assert.equal(relayPairingSaved(loadRelayCredentials(hash, storage), storage), true);
  assert.equal(relayPairingSaved(credentials), false);
  assert.equal(relayPairingSaved(null, storage), false);
  const blocked = {
    getItem: () => { throw new Error('blocked'); },
    setItem: () => { throw new Error('blocked'); },
    removeItem: () => { throw new Error('blocked'); },
  };
  assert.deepEqual(loadRelayCredentials(hash, blocked), credentials);
  assert.equal(relayPairingSaved(credentials, blocked), false, 'retain the original fragment for reload');
  const different = { ...credentials, route: 'another-machine-route-12345' };
  const readOnly = { getItem: () => JSON.stringify(different), setItem: () => { throw new Error('quota'); }, removeItem: () => { throw new Error('readonly'); } };
  assert.deepEqual(loadRelayCredentials(hash, readOnly), credentials);
  assert.equal(relayPairingSaved(credentials, readOnly), false, 'never reload into the old machine');
});


test('compact pairing restores the original credentials and survives reload', () => {
  const original = { route: 'c0058cf6c0f274dfe7994b84ee2a4b5097f1ade1c510da538065cd367e69fac1', token: 'fa3ea824a5ef913362a3f552fdea1cb9' };
  // Fixed fixture shared with the Rust URL producer.
  const hash = '#r=wAWM9sDydN_nmUuE7ipLUJfxreHFENpTgGXNNn5p-sE&t=-j6oJKXvkTNio_VS_eocuQ';
  const saved = new Map<string, string>();
  const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value); }, removeItem: (key: string) => { saved.delete(key); } };
  assert.deepEqual(loadRelayCredentials(hash, storage), original);
  assert.deepEqual(loadRelayCredentials('', storage), original);
  assert.equal(relayPairingSaved(original, storage), true);
  const [, init] = relayRequest(loadRelayCredentials(hash)!, '/project');
  assert.equal(new Headers(init.headers).get('X-Atom-Token'), original.route);
  assert.equal(new Headers(init.headers).get('Authorization'), 'Bearer ' + original.token);
});

test('malformed or mixed compact links fail closed instead of reusing stored credentials', () => {
  const storage = { getItem: () => JSON.stringify(credentials), removeItem: () => {} };
  for (const hash of ['#r=bad&t=bad', '#t=-j6oJKXvkTNio_VS_eocuQ', '#r=AAAAAAAAAAA&t=AAAAAAAAAAB', '#r=AAAAAAAAAAA&t=AAAAAAAAAAA&relay=old', '#r=AAAAAAAAAAA&t=AAAAAAAAAAA&token=old']) {
    assert.equal(loadRelayCredentials(hash, storage), null, hash);
  }
});
