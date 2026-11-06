// Real provider form with a mock daemon. Run after npm run build.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const clients = new Set(), requests = [];
let writes = 0, pending;
let answer = { error: 'Could not connect to Ollama at http://localhost:11434. Run `ollama serve` on the machine running atomcode, then retry model discovery. Default port: 11434.' };
const initialAnswer = answer;
const preset = (id, type, url) => ({ id, display_name: id, type, default_base_url: url, requires_api_key: type !== 'ollama' });
const config = { providers: [], provider_accounts: [], provider_presets: [preset('OpenAI', 'openai', 'https://api.openai.com/v1'), preset('Ollama', 'ollama', 'http://localhost:11434')], provider_protocols: [] };
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://fixture').pathname;
  const json = value => { res.writeHead(value.error ? 502 : 200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value)); };
  if (path === '/live') { res.writeHead(200, { 'Content-Type': 'text/event-stream' }); res.write('data: {"type":"state","running":false}\n\n'); clients.add(res); req.on('close', () => clients.delete(res)); return; }
  if (path === '/providers/discover-models') {
    let body = ''; for await (const chunk of req) body += chunk; requests.push(JSON.parse(body));
    if (pending) { const held = pending; pending = undefined; held(value => json(value)); return; }
    return json(answer);
  }
  if (['POST', 'PUT', 'PATCH', 'DELETE'].includes(req.method)) { writes++; return json({}); }
  if (path === '/config') return json(config);
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (['/models', '/projects', '/sessions', '/skills', '/chat/active'].includes(path)) return json([]);
  if (path === '/live/mode') return json({ mode: 'build' });
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    const data = await readFile(file); res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream'); res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.OLLAMA_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1200, height: 1000 } });
const errors = []; page.on('pageerror', error => errors.push(error.message));
try {
 for (const width of [1200, 390]) {
  requests.length = 0; answer = initialAnswer;
  await page.setViewportSize({width, height:1000});
  await page.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  if (width < 768) await page.locator('.mobile-menu-btn').click();
  await page.locator('.sidebar-settings-btn:visible, .rail-btn-settings:visible').first().click();
  await page.getByRole('button', { name: 'Model configuration', exact: true }).click();
  await page.getByRole('button', { name: 'Add model provider' }).click();
  const form = page.locator('.add-provider-card');
  // A draft must survive accidental backdrop clicks and Escape.
  const key = form.locator('input[type=password]').first();
  await key.fill('unsaved-fixture-key');
  await page.mouse.click(5, 5);
  assert(await form.isVisible(), 'backdrop must not dismiss the provider form');
  await key.press('Escape');
  assert(await form.isVisible(), 'Escape must not dismiss the provider form');
  assert.equal(await key.inputValue(), 'unsaved-fixture-key');
  await page.screenshot({path:`/tmp/atomcode-modal-provider-${width}.png`});
  await key.fill('');
  const choose = async name => { await form.locator('.custom-select-trigger').first().click(); await form.locator('.custom-select-option').filter({ hasText: name }).click(); };
  await choose('Ollama');
  await form.getByRole('alert').filter({ hasText: 'ollama serve' }).waitFor();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].base_url, 'http://localhost:11434');
  assert.equal(writes, 0);
  await page.screenshot({ path: '/tmp/atomcode-ollama-guidance.png' });
  answer = { models: [{ id: 'local-model' }] };
  await form.getByRole('button', { name: 'Retry model discovery' }).click();
  const picker = page.locator('.model-picker-modal');
  await picker.getByText('local-model', { exact: true }).waitFor();
  await page.mouse.click(5, 5);
  assert(await picker.isVisible(), 'backdrop must not dismiss the nested picker');
  await picker.locator('button').first().focus();
  await page.keyboard.press('Escape');
  assert(await picker.isVisible(), 'Escape must not dismiss the nested picker');
  assert(await form.isVisible(), 'the parent dialog remains open');
  await picker.getByRole('button', { name: 'Cancel', exact: true }).click();
  assert.equal(requests.length, 2);
  let release;
  const held = new Promise(resolve => { pending = callback => { release = callback; resolve(); }; });
  await form.getByRole('button', { name: 'Fetch available models' }).click(); await held;
  await form.locator('summary').click();
  await form.getByRole('textbox', { name: 'Base URL', exact: true }).fill('http://localhost:12345');
  release({ models: [{ id: 'stale-model' }] });
  answer = { models: [{ id: 'new-address-model' }] };
  await picker.getByText('new-address-model', { exact: true }).waitFor();
  assert.equal(await page.getByText('stale-model', { exact: true }).count(), 0);
  assert.equal(requests.at(-1).base_url, 'http://localhost:12345');
  await picker.getByRole('button', { name: 'Cancel', exact: true }).click();
  const heldAgain = new Promise(resolve => { pending = callback => { release = callback; resolve(); }; });
  await form.getByRole('button', { name: 'Fetch available models' }).click(); await heldAgain;
  await choose('OpenAI'); release({ error: 'stale-ollama-failure' });
  await page.waitForTimeout(400);
  assert.equal(await page.getByText('stale-ollama-failure', { exact: true }).count(), 0);
  // A manual click before the automatic debounce fires must make just one request.
  const beforeManual = requests.length;
  answer = { error: 'manual-discovery-failure' };
  await choose('Ollama');
  await form.getByRole('button', { name: 'Fetch available models' }).click();
  await form.getByRole('alert').filter({ hasText: 'manual-discovery-failure' }).waitFor();
  await page.waitForTimeout(400);
  assert.equal(requests.length, beforeManual + 1);
  assert.equal(writes, 0); assert.deepEqual(errors, []);
  await page.locator('.model-config-modal > .modal-header .modal-close').click();
  await form.waitFor({state:'detached'});

 }
  console.log('PASS: desktop/mobile modal dismissal, draft preservation, automatic discovery, retry, stale responses, no implicit writes');
} catch (error) {
  await page.screenshot({ path: '/tmp/atomcode-ollama-failure.png' });
  console.error((await page.locator('body').innerText()).slice(-4000)); throw error;
} finally {
  await browser.close(); for (const client of clients) client.end(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
}
