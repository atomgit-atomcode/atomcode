import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import { chromium } from 'playwright';

const root = resolve(import.meta.dirname, '../dist');
const id = 'recap-fixture';
const other = 'second-fixture';
const meta = (sessionId) => ({
  id: sessionId, name: sessionId, working_dir: '/fixture', project_hash: 'fixture',
  created_at: 1700000000, updated_at: 1700000001, message_count: 2,
});
const history = [
  { role: 'user', content: 'Earlier question' },
  { role: 'assistant', content: 'Earlier answer' },
];
let live;
let chat;
let liveSession = id;
const requests = new Map();
const emit = (response, event) => response.write(`data: ${JSON.stringify(event)}\n\n`);

const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = (value) => {
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(value));
  };
  if (path === '/live') {
    live = res;
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    emit(res, { type: 'snapshot', session_id: liveSession, session_name: liveSession,
      project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture',
      model: 'fixture', mode: 'build', messages: history });
    emit(res, { type: 'state', running: false });
    return;
  }
  if (path === '/chat') {
    chat = res;
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    res.write(': connected\n\n');
    return;
  }
  if (path === '/chat/recap') {
    let body = '';
    for await (const chunk of req) body += chunk;
    const request = JSON.parse(body);
    requests.set(request.turn_id, { request, respond: (text) => json({ text }) });
    return;
  }
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (path.startsWith('/sessions/resolve/')) return json(meta(path.split('/').at(-1)));
  if (path.endsWith('/transcript')) return json({ session_id: liveSession, turns: [] });
  if (path.startsWith('/projects/fixture/sessions/')) {
    return json({ ...meta(path.split('/').at(-1)), messages: history });
  }
  if (path === '/live/switch_session') return json({ ok: true, session_id: liveSession });
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta(id), meta(other)]);
  if (path === '/chat/active' || path === '/projects' || path === '/skills' || path.startsWith('/workbench/')) return json([]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    assert.ok(file.startsWith(root + '/'));
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const browser = await chromium.launch({
  executablePath: process.env.CONTEXT_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  headless: true,
});
const stats = (turnId, duration = 120_000) => ({
  turn_id: turnId, duration_ms: duration, rounds: 1, tool_calls: 0,
  prompt_tokens: 10, completion_tokens: 5, cached_tokens: 0,
});

async function awaitRequest(turnId) {
  for (let attempt = 0; attempt < 100 && !requests.has(turnId); attempt++) {
    await new Promise((done) => setTimeout(done, 20));
  }
  const captured = requests.get(turnId);
  assert.deepEqual(captured?.request, { session_id: id, turn_id: turnId });
  return captured.respond;
}

try {
  for (const [sync, width] of [[true, 1200], [false, 390]]) {
    live = chat = undefined;
    liveSession = id;
    requests.clear();
    const page = await browser.newPage({ viewport: { width, height: 840 }, hasTouch: !sync });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.addInitScript(() => {
      localStorage.setItem('atomcode.lang', 'en');
      localStorage.setItem('atomcode.onboarding.done', '1');
      localStorage.setItem('atomcode.sidebar.view.v1', 'flat');
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=${sync ? 1 : 0}&session=${id}`);
    await page.getByText('Earlier answer', { exact: true }).waitFor();
    const input = page.locator('.message-input');
    const start = async (turnId) => {
      if (sync) {
        emit(live, { type: 'user', text: `Question ${turnId}` });
        emit(live, { type: 'state', running: true });
      } else {
        await input.fill(`Question ${turnId}`);
        await Promise.all([page.waitForResponse((response) => new URL(response.url()).pathname === '/chat'),
          page.locator('.btn-send').click()]);
      }
      emit(sync ? live : chat, { type: 'text', content: `Answer ${turnId}` });
      await page.getByText(`Answer ${turnId}`, { exact: true }).waitFor();
    };
    const finish = (turnId, duration, reason = 'stopped') => {
      const response = sync ? live : chat;
      emit(response, { ...(sync ? { type: 'state', running: false } : { type: 'done', session_id: id }),
        stop_reason: reason, stats: stats(turnId, duration) });
      if (!sync) response.end();
    };

    await start(1);
    finish(1, 100);
    await page.locator('.btn-stop').waitFor({ state: 'hidden' });
    await start(2);
    if (!sync) {
      // The page went away and came back during this short turn.
      await page.evaluate(() => {
        Object.defineProperty(document, 'hidden', { configurable: true, value: true });
        document.dispatchEvent(new Event('visibilitychange'));
        delete document.hidden;
        document.dispatchEvent(new Event('visibilitychange'));
      });
    }
    finish(2, sync ? 120_000 : 100);
    const respond = await awaitRequest(2);
    assert.equal(requests.has(1), false);
    assert.equal(await page.locator('.turn-recap').count(), 0);
    await page.locator('.btn-stop').waitFor({ state: 'hidden' });
    await start(3);
    respond('Work complete. ' + 'Pending confirmation. '.repeat(8));
    const recap = page.locator('.turn-recap');
    await recap.waitFor();
    assert.match(await recap.textContent(), /^Recap · Work complete/);
    assert.match(await recap.locator('..').textContent(), /Answer 2/);
    assert.ok(await recap.evaluate((node) => node.scrollWidth <= node.clientWidth));
    finish(3, 120_000, 'cancelled');
    await page.locator('.btn-stop').waitFor({ state: 'hidden' });
    assert.equal(requests.has(3), false);

    if (sync) {
      emit(live, { type: 'loop_changed', active: true, round: 1, elapsed_secs: 0, label: 'Fixture' });
      await start(4);
      emit(live, { type: 'loop_changed', active: false, round: 1, elapsed_secs: 120, label: 'Fixture' });
      finish(4, 120_000);
      await page.locator('.btn-stop').waitFor({ state: 'hidden' });
      await start(5);
      finish(5, 120_000);
      const respondLate = await awaitRequest(5);
      assert.equal(requests.has(4), false);
      liveSession = other;
      emit(live, { type: 'session_switched', session_id: other });
      await page.locator('.session-title-text').filter({ hasText: other }).waitFor();
      const lateResponse = page.waitForResponse((response) => response.url().endsWith('/chat/recap'));
      respondLate('Late recap from the previous session');
      await lateResponse;
      assert.equal(await page.getByText('Late recap from the previous session').count(), 0);
    }

    assert.deepEqual(errors, []);
    await page.close();
  }
  console.log('PASS: live and detached recaps, short and cancelled turns, autonomy, tab-away, narrow layout and late session switch');
} finally {
  await browser.close();
  server.closeAllConnections();
  server.close();
}
