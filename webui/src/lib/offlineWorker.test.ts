import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';

/** Load `public/sw.js` against a fake worker scope, with `fetch` scripted. */
function worker(fetchImpl: (request: unknown) => Promise<Response>) {
  const listeners: Record<string, (event: unknown) => void> = {};
  const self = {
    addEventListener: (type: string, fn: (event: unknown) => void) => {
      listeners[type] = fn;
    },
    skipWaiting: () => undefined,
    clients: { claim: () => Promise.resolve() },
  };
  const source = readFileSync(join(process.cwd(), 'public/sw.js'), 'utf8');
  runInNewContext(source, { self, fetch: fetchImpl, Response });
  return (mode: string, language = 'zh-CN,zh;q=0.9') => {
    let answered: Promise<Response> | undefined;
    listeners.fetch({
      request: { mode, headers: new Headers({ 'Accept-Language': language }) },
      respondWith: (p: Promise<Response>) => {
        answered = p;
      },
    });
    return answered;
  };
}

test('a page load that cannot reach the webui gets a page saying how to start it', async () => {
  const load = worker(() => Promise.reject(new TypeError('connection refused')));
  const zh = await load('navigate')!;
  assert.equal(zh.status, 503);
  const zhPage = await zh.text();
  assert.match(zhPage, /AtomCode webui 没在运行/);
  assert.match(zhPage, /atomcode webui/);
  assert.match(zhPage, /\/webui/);
  const en = await load('navigate', 'en-US,en;q=0.9')!;
  assert.match(await en.text(), /AtomCode webui is not running/);
});

test('when the webui is running the page comes from it, untouched', async () => {
  const served = new Response('<html>app</html>', { status: 200 });
  const load = worker(() => Promise.resolve(served));
  assert.equal(await load('navigate'), served);
});

test('nothing but page loads goes through the worker', () => {
  const load = worker(() => Promise.reject(new TypeError('unreachable')));
  for (const mode of ['cors', 'same-origin', 'no-cors']) {
    assert.equal(load(mode), undefined, `${mode} requests are left to the browser`);
  }
});
