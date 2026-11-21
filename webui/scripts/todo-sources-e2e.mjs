// Real component; source transport is local and no session is accessed.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), server: { host: '127.0.0.1', port: 0 } });
let browser;
try {
  await server.listen();
  browser = await chromium.launch({ executablePath: process.env.TODO_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
  const page = await browser.newPage();
  const errors = []; page.on('pageerror', error => errors.push(error.message));
  const items = [{ content: 'Apply fix', status: 'in_progress' }];
  const raw = '<script>window.sourceExecuted=true</script>\r\n```diff\r\n@@ -1 +1 @@\r\n-old\r\n+新\r\n```';
  let fail = false; let empty = false; let requests = 0; let delay = null; let observeRequest = null;
  await page.route('**/todo-sources**', async route => {
    requests++; observeRequest?.(); observeRequest = null;
    const url = new URL(route.request().url());
    if (delay) await delay;
    if (fail) { await route.fulfill({ status: 503, json: { error: 'Source lookup unavailable' } }); return; }
    const offset = Number(url.searchParams.get('offset') ?? 0);
    const result = url.searchParams.has('source_seq') ? {
      batch_seq: 3, batch_index: 0, sources: [], todos: [],
      page: { seq: 1, session_id: 'session-a', offset, text: offset === 0 ? raw : 'second page', total_chars: 5000, next_offset: offset === 0 ? 4096 : null, reference_notice: 'Historical reference' },
    } : { batch_seq: 3, batch_index: 0, sources: empty ? [] : [{ seq: 1, chars: 5000, preview: 'guidance' }], todos: items };
    await route.fulfill({ json: result }).catch(() => {});
  });
  await page.route('**/__todo-source-test', async route => route.fulfill({ contentType: 'text/html', body: await server.transformIndexHtml('/__todo-source-test', `<!doctype html><html><body><div id="root"></div><script type="module">
    import {h,render} from 'preact';
    import {SettingsProvider} from '/src/settings.tsx';
    import {TodoGuidanceSources} from '/src/components/TodoGuidanceSources.tsx';
    localStorage.setItem('atomcode.lang','en');
    window.mountSource = (id='session-a', revision=0) => render(h(SettingsProvider,null,h(TodoGuidanceSources,{projectHash:'fixture',sessionId:id,items:${JSON.stringify(items)},revision})),document.getElementById('root'));
    window.mountSource();
  </script></body></html>`) }));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/__todo-source-test`);
  const open = page.locator('.todo-guidance-sources > button');
  await open.waitFor(); assert.equal(requests, 1, 'discover source metadata before showing the entry');
  assert.match(await open.textContent(), /Original requests · 1/);
  assert.equal(await page.locator('.todo-source-hint').count(), 0);
  await open.click();
  const source = page.locator('.todo-guidance-sources button').filter({ hasText: /#1/ });
  await source.waitFor(); await source.click();
  await page.locator('pre').waitFor();
  assert.equal(await page.locator('pre').textContent(), raw);
  assert.equal(await page.evaluate(() => window.sourceExecuted), undefined, 'original text is escaped');
  await page.getByRole('button', { name: 'Next', exact: true }).click();
  await page.getByText('second page', { exact: true }).waitFor();
  await page.getByRole('button', { name: 'Previous', exact: true }).click();
  await page.waitForFunction(text => document.querySelector('pre')?.textContent === text, raw);
  // Replacing a batch with identical tasks must also invalidate its old page.
  for (const [session, revision] of [['session-a', 1], ['session-b', 2]]) {
    let release; delay = new Promise(resolve => { release = resolve; });
    const observed = new Promise(resolve => { observeRequest = resolve; });
    await source.click(); await observed;
    await page.evaluate(([id, version]) => window.mountSource(id, version), [session, revision]);
    release(); delay = null;
    await page.waitForFunction(() => document.querySelector('.todo-guidance-sources > button')?.getAttribute('aria-expanded') === 'false');
    assert.equal(await page.locator('pre').count(), 0, 'batch/session switch clears stale source pages');
    if (session === 'session-a') {
      await open.click(); await source.waitFor(); await source.click(); await page.locator('pre').waitFor();
    }
  }
  empty = true;
  const emptyLoaded = page.waitForResponse(response => response.url().includes('/todo-sources'));
  await page.evaluate(() => window.mountSource('empty-session', 3));
  await emptyLoaded;
  await page.waitForFunction(() => !document.querySelector('.todo-guidance-sources'));
  assert.equal(await page.locator('.todo-guidance-sources').count(), 0, 'no linked sources means no entry or empty panel');
  fail = true;
  await page.evaluate(() => window.mountSource('failed-session', 4));
  await page.getByRole('alert').waitFor();
  assert.match(await page.getByRole('alert').textContent(), /Source lookup unavailable/);
  fail = false; empty = false;
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await open.waitFor();
  assert.equal(await page.locator('.todo-source-hint').count(), 0, 'retry preserves collapsed discovery');
  assert.deepEqual(errors, []);
  console.log('Todo guidance: lazy read, original text, pagination, session reset passed');
} finally { await browser?.close(); await server.close(); }
