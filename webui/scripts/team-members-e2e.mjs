// Exercise the production app with synthetic session and member responses.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import { chromium } from 'playwright';

const dist = resolve(import.meta.dirname, '../dist');
const id = 'team-lead-fixture';
const hash = '0123456789abcdef';
const member = { id: id + '~expert2', name: 'expert2', role: 'expert', activity: 'open' };
const meta = { id, name: 'Team fixture conversation', working_dir: '/fixture', project_hash: hash, created_at: 1, updated_at: 2, message_count: 2 };
const messages = [{ role: 'user', content: 'Main request' }, { role: 'assistant', content: 'Main response' }];
const project = `/projects/${hash}/sessions`;
const history = `${project}/${id}`;
let roster = [member];
const switchTargets = [];
const streams = new Set();

const server = createServer(async (req, res) => {
  const route = new URL(req.url, 'http://localhost').pathname;
  const json = value => {
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(value));
  };
  if (route === history + '/team') return json(roster);
  if (route === `${history}/team/${member.id}`) {
    return json({ entries: [{ seq: 1, kind: 'assistant', text: 'Member response' }], truncated: false });
  }
  if (route === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    res.write(`data: ${JSON.stringify({
      type: 'snapshot', session_id: id, session_name: meta.name, project_hash: hash,
      working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', messages,
    })}\n\ndata: {"type":"state","running":false}\n\n`);
    streams.add(res);
    res.on('close', () => streams.delete(res));
    return;
  }
  if (route === '/live/switch_session') {
    let body = '';
    for await (const chunk of req) body += chunk;
    switchTargets.push(JSON.parse(body).session_id);
    return json({ ok: true });
  }
  if (route === '/live/message') return json({ accepted: true, disposition: 'started', generation: 1, turn_id: 1 });
  if (route === '/live/mode') return json({ mode: 'build' });
  if (route === '/live/session') return json({ success: true, session_id: id });
  if (route === '/project') return json({ working_dir: '/fixture', project_hash: hash });
  if (route === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (route === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (route.startsWith('/sessions/resolve/')) return json(meta);
  if (route === history) return json({ ...meta, messages });
  if (route.endsWith('/transcript')) return json({ session_id: id, turns: [] });
  if (route === project || route === '/sessions') return json([meta]);
  if (['/chat/active', '/projects', '/skills'].includes(route)) return json([]);
  try {
    const file = resolve(dist, route === '/' ? 'index.html' : '.' + route);
    if (!file.startsWith(dist + '/')) throw new Error('Outside dist');
    const contents = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(contents);
  } catch {
    res.writeHead(404).end();
  }
});

await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({
  executablePath: process.env.TEAM_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  headless: true,
});
try {
  for (const width of [1200, 390, 320]) {
    const page = await browser.newPage({ viewport: { width, height: 850 } });
    page.setDefaultTimeout(6000);
    await page.addInitScript(() => {
      localStorage.setItem('atomcode.lang', 'en');
      localStorage.setItem('atomcode.onboarding.done', '1');
    });
    await page.goto(`${base}/?sync=1&session=${id.slice(0, 8)}`);
    const trigger = page.getByRole('button', { name: 'Team · 1' });
    await trigger.waitFor();
    await trigger.click();
    await page.getByRole('button', { name: 'expert2 Idle' }).click();
    const panel = page.getByRole('complementary', { name: 'expert2' });
    await panel.getByText('Member response').waitFor();
    assert.equal(await panel.locator('textarea').count(), 0);
    await page.getByText('Main response').waitFor();
    const box = await panel.boundingBox();
    if (width === 1200) {
      const composer = page.locator('.message-input');
      const input = await composer.boundingBox();
      assert(input.x + input.width <= box.x + 1);
      await composer.fill('Send to the lead');
      const sent = page.waitForRequest(req => req.url().endsWith('/live/message'));
      await page.locator('.btn-send').click();
      assert.equal((await sent).postDataJSON().session_id, id);
    } else {
      assert.equal(Math.round(box.y + box.height), 850);
      assert.equal(Math.round(box.width), width);
    }
    await panel.getByRole('button', { name: 'Close', exact: true }).click();
    await panel.waitFor({ state: 'detached' });
    await page.close();
  }

  roster = [];
  const page = await browser.newPage();
  const rosterResponse = page.waitForResponse(response => response.url().endsWith('/team'));
  await page.goto(`${base}/?sync=1&session=${id.slice(0, 8)}`);
  await rosterResponse;
  assert.equal(await page.locator('.team-trigger').count(), 0);
  assert(switchTargets.every(target => target === id));
  console.log('Team member browser scenarios passed');
} finally {
  await browser.close();
  for (const stream of streams) stream.end();
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}
