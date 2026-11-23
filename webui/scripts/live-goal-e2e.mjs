// The real WebUI against a local daemon fixture.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import { chromium } from 'playwright';

const dist = resolve(import.meta.dirname, '../dist');
const first = { id: 'goal-session', name: 'Goal session', working_dir: '/fixture', project_hash: 'fixture' };
const second = { ...first, id: 'other-session', name: 'Other session' };
const sessions = [first, second];
const messages = [{ role: 'user', content: 'Existing conversation' }];
const condition = 'Verify regressions and keep the existing composer usable across the entire workflow';
const goalId = 'eb19ea4a-c2ef-4ee0-9936-5116afe6f71c';
const running = { control_id: goalId, condition, active: true, phase: 'pursuing', elapsed_secs: 3692, last_reason: null };
const requests = [];
const clients = new Set();
let session = first;
let goal = running;
let rejectAction = false;
const emit = (res, event) => res.write(`data: ${JSON.stringify(event)}\n\n`);
const publish = event => clients.forEach(res => emit(res, event));
const publishGoal = progress => {
  goal = progress;
  publish({ type: 'goal_changed', ...progress });
};

const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => {
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(value));
  };
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    emit(res, {
      type: 'snapshot', session_id: session.id, project_hash: 'fixture',
      provider: 'fixture', model: 'fixture', mode: 'build', messages, goal,
    });
    emit(res, { type: 'state', running: false });
    clients.add(res);
    req.on('close', () => clients.delete(res));
    return;
  }
  if (path === '/live/goal/action') {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    requests.push(body);
    if (rejectAction) return json({ accepted: false, error: 'The goal has changed' });
    const paused = body.action === 'pause';
    publishGoal({ ...running, active: false, phase: paused ? 'paused' : 'ended', elapsed_secs: 3695 });
    return json({ accepted: true });
  }
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ provider: 'fixture', model: 'fixture' }]);
  if (path === '/live/session') return json({ success: true, session_id: session.id });
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path.endsWith('/transcript')) return json({ session_id: session.id, turns: [] });
  if (path.startsWith('/sessions/resolve/')) return json(sessions.find(s => path.endsWith(s.id)));
  if (path.startsWith('/projects/fixture/sessions/')) return json({
    ...sessions.find(s => path.endsWith(s.id)), messages,
  });
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json(sessions);
  if (['/projects', '/skills', '/chat/active'].includes(path)) return json([]);
  try {
    const file = resolve(dist, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(dist + '/')) throw new Error('outside dist');
    const bytes = await readFile(file);
    res.setHeader('Content-Type', ({
      '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript',
    })[extname(file)] ?? 'application/octet-stream');
    res.end(bytes);
  } catch {
    res.writeHead(404).end();
  }
});

await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
let browser;
try {
  browser = await chromium.launch({
    executablePath: process.env.LIVE_GOAL_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    headless: true,
  });
  for (const width of [1200, 390, 320]) {
    session = first;
    goal = running;
    const page = await browser.newPage({ viewport: { width, height: 850 } });
    page.setDefaultTimeout(7000);
    await page.clock.install();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('atomcode.lang', 'en'));
    await page.goto(`${base}/?sync=1&session=${first.id}`);
    const bar = page.locator('.live-goal-bar');
    const input = page.locator('textarea.message-input');
    await bar.waitFor();
    await input.fill('Unsaved draft');
    assert.equal(await bar.getByText(condition, { exact: true }).count(), 1);
    assert.deepEqual(await bar.getByRole('button').allTextContents(), ['Pause', 'Delete']);
    const elapsed = bar.locator('.live-goal-elapsed');
    await elapsed.waitFor();
    const firstElapsed = await elapsed.textContent();
    assert.match(firstElapsed, /^1h 1m \d{2}s$/);
    await page.clock.runFor(2000);
    await page.waitForFunction(previous => {
      const current = document.querySelector('.live-goal-elapsed')?.textContent;
      return current && current > previous;
    }, firstElapsed);
    const barBox = await bar.boundingBox();
    const inputBox = await input.boundingBox();
    assert.ok(barBox.y + barBox.height <= inputBox.y);
    assert.ok(barBox.x >= 0 && barBox.x + barBox.width <= width);
    assert.ok(barBox.height < 60);
    if (width <= 420) {
      assert.ok(await bar.locator('.live-goal-condition').evaluate(el => el.scrollWidth > el.clientWidth));
      const actions = await bar.locator('.live-goal-actions').boundingBox();
      assert.ok(actions.x + actions.width <= barBox.x + barBox.width);
    }

    const pause = bar.getByRole('button', { name: 'Pause', exact: true });
    const remove = bar.getByRole('button', { name: 'Delete', exact: true });
    await pause.click();
    await bar.getByRole('status').filter({ hasText: 'Goal paused' }).waitFor();
    assert.equal(await elapsed.textContent(), '1h 1m 35s');
    await page.clock.runFor(2200);
    assert.equal(await elapsed.textContent(), '1h 1m 35s');
    assert.equal(await pause.isDisabled(), true);
    assert.deepEqual(requests.at(-1), { session_id: first.id, control_id: goalId, action: 'pause' });

    rejectAction = true;
    await remove.click();
    await bar.getByRole('alert').filter({ hasText: 'The goal has changed' }).waitFor();
    rejectAction = false;
    await remove.click();
    await bar.waitFor({ state: 'hidden' });
    assert.deepEqual(requests.at(-1), { session_id: first.id, control_id: goalId, action: 'remove' });
    assert.equal(await input.inputValue(), 'Unsaved draft');
    assert.equal(await page.getByText('Existing conversation', { exact: true }).count(), 1);

    publishGoal({ ...running, active: false, phase: 'paused_at_cap', last_reason: 'Fixture cap' });
    await bar.waitFor();
    await page.locator('.goal-recovery-status').filter({ hasText: 'Fixture cap' }).waitFor();
    session = second;
    goal = null;
    publish({ type: 'session_switched', session_id: second.id });
    await bar.waitFor({ state: 'hidden' });
    assert.deepEqual(errors, []);
    await page.close();
  }
  console.log('Live Goal: desktop and mobile layout, pause, delete, failure, draft and session isolation passed');
} finally {
  await browser?.close();
  for (const client of clients) client.end();
  await new Promise(resolve => {
    server.close(resolve);
    server.closeAllConnections();
  });
}
