// Real provider UI and a mock daemon; no provider credentials or external calls.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';

const root = resolve(import.meta.dirname, '../dist');
const clients = new Set(), discoveries = [], writes = [];
const connections = [
  { type: 'openai', base_urls: ['https://taotoken.net/api/v1', 'https://api.taotoken.net/coding/v1', 'https://api.taotoken.net/token/v1'] },
  { type: 'anthropic', base_urls: ['https://taotoken.net/api', 'https://api.taotoken.net/coding', 'https://api.taotoken.net/token'] },
];
const config = { providers: [], provider_accounts: [], provider_presets: [{ id: 'taotoken', display_name: 'TaoToken', type: 'openai', default_base_url: connections[0].base_urls[0], requires_api_key: true, discoverable: true, connections }], provider_protocols: [] };
config.provider_presets.push({ id: 'aliyun', display_name: 'Alibaba', type: 'openai', default_base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1', requires_api_key: true, discoverable: true, connections: [
  { type: 'openai', base_urls: ['https://dashscope.aliyuncs.com/compatible-mode/v1'] },
  { type: 'anthropic', base_urls: ['https://dashscope.aliyuncs.com/apps/anthropic'] },
] });
config.provider_presets.push({ id: 'anthropic', display_name: 'Anthropic Official', type: 'anthropic', default_base_url: 'https://api.anthropic.com', requires_api_key: true, discoverable: true });
config.provider_presets.push({ id: 'zhipu-coding', display_name: 'Zhipu Coding', type: 'openai', default_base_url: 'https://open.bigmodel.cn/api/coding/paas/v4', requires_api_key: true, discoverable: true, connections: [
  { type: 'openai', base_urls: ['https://open.bigmodel.cn/api/coding/paas/v4'] },
  { type: 'anthropic', base_urls: ['https://open.bigmodel.cn/api/anthropic'] },
] });
const addedPresets = [
  ['aliyun-token-plan', 'Alibaba Cloud Token Plan', 'https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1', 'https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic'],
  ['aliyun-coding', 'Alibaba Cloud Coding Plan', 'https://coding.dashscope.aliyuncs.com/v1', 'https://coding.dashscope.aliyuncs.com/apps/anthropic'],
  ['xiaomi-token-plan', 'Xiaomi MiMo Token Plan', 'https://token-plan-cn.xiaomimimo.com/v1', 'https://token-plan-cn.xiaomimimo.com/anthropic'],
  ['kimi-code', 'Kimi Code', 'https://api.kimi.com/coding/v1', 'https://api.kimi.com/coding'],
  ['tencent-tokenhub', 'Tencent TokenHub', 'https://tokenhub.tencentmaas.com/v1', 'https://tokenhub.tencentmaas.com'],
  ['tencent-token-plan', 'Tencent Token Plan', 'https://tokenhub.tencentmaas.com/plan/v3', 'https://tokenhub.tencentmaas.com/plan/anthropic'],
  ['tencent-coding', 'Tencent Coding Plan', 'https://api.lkeap.cloud.tencent.com/coding/v3', 'https://api.lkeap.cloud.tencent.com/coding/anthropic'],
];
for (const [id, display_name, openai, anthropic] of addedPresets) {
  config.provider_presets.push({ id, display_name, type: 'openai', default_base_url: openai, requires_api_key: true, discoverable: true,
    connections: anthropic ? [{ type: 'openai', base_urls: [openai] }, { type: 'anthropic', base_urls: [anthropic] }] : [] });
}
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://fixture').pathname;
  const json = value => { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value)); };
  if (path === '/live') { res.writeHead(200, { 'Content-Type': 'text/event-stream' }); res.write('data: {"type":"state","running":false}\n\n'); clients.add(res); req.on('close', () => clients.delete(res)); return; }
  if (path === '/providers/discover-models' || path === '/provider-accounts') {
    let body = ''; for await (const chunk of req) body += chunk;
    if (path === '/providers/discover-models') { discoveries.push(JSON.parse(body)); return json({ models: [{ id: 'listed-model' }] }); }
    writes.push(JSON.parse(body)); return json({ account: 'taotoken', models: ['taotoken/manual-model'], config });
  }
  if (path === '/config') return json(config);
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (['/models', '/projects', '/sessions', '/skills', '/chat/active'].includes(path)) return json([]);
  if (path === '/live/mode') return json({ mode: 'build' });
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.PROVIDER_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const page = await browser.newPage();
const errors = []; page.on('pageerror', error => errors.push(error.message));
try {
  for (const width of [1200, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await page.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    if (width < 768) await page.locator('.mobile-menu-btn').click();
    await page.locator('.sidebar-settings-btn:visible, .rail-btn-settings:visible').first().click();
    await page.getByRole('button', { name: 'Model configuration', exact: true }).click();
    await page.getByRole('button', { name: 'Add model provider' }).click();
    const form = page.locator('.add-provider-card');
    const endpoint = form.getByRole('textbox', { name: 'Base URL', exact: true });
    const model = form.getByRole('textbox', { name: 'Model ID', exact: true });
    const key = form.locator('input[type="password"]').first();
    const select = async name => {
      await form.locator('.custom-select-trigger').nth(1).click();
      await form.locator('.custom-select-option').getByText(name, { exact: true }).click();
    };
    await endpoint.waitFor(); // Visible without opening advanced settings.
    for (const [, name, openai, anthropic] of addedPresets) {
      await form.locator('.custom-select-trigger').first().click();
      await form.locator('.custom-select-option').filter({ hasText: name }).click();
      assert.equal(await endpoint.inputValue(), openai, name);
      await key.fill('fixture-plan-key');
      if (anthropic) {
        await select('Anthropic');
        assert.equal(await endpoint.inputValue(), anthropic, name);
        assert.equal(await key.inputValue(), 'fixture-plan-key');
        await select('OpenAI');
        assert.equal(await endpoint.inputValue(), openai, name);
      }
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), name);
    }

    await form.locator('.custom-select-trigger').first().click();
    await form.locator('.custom-select-option').filter({ hasText: 'Anthropic Official' }).click();
    assert.equal(await endpoint.inputValue(), 'https://api.anthropic.com');
    assert.equal(await form.locator('.add-model-field').filter({ has: page.locator('.add-model-label', { hasText: /^Protocol$/ }) }).innerText(), 'Protocol\nAnthropic');
    await form.locator('.custom-select-trigger').first().click();
    await form.locator('.custom-select-option').filter({ hasText: 'Zhipu Coding' }).click();
    await select('Anthropic');
    assert.equal(await endpoint.inputValue(), 'https://open.bigmodel.cn/api/anthropic');
    await select('OpenAI');
    assert.equal(await endpoint.inputValue(), 'https://open.bigmodel.cn/api/coding/paas/v4');
    await form.locator('.custom-select-trigger').first().click();
    await form.locator('.custom-select-option').filter({ hasText: 'TaoToken' }).click();
    assert.equal(await endpoint.inputValue(), connections[0].base_urls[0]);
    await key.fill('fixture-key');
    await model.fill('old-openai-model');
    await endpoint.fill(connections[0].base_urls[1]);
    await select('Anthropic');
    assert.equal(await endpoint.inputValue(), connections[1].base_urls[1]);
    assert.equal(await key.inputValue(), 'fixture-key');
    assert.equal(await model.inputValue(), '', 'staged models must be reselected for a new protocol');
    await select('OpenAI');
    assert.equal(await endpoint.inputValue(), connections[0].base_urls[1]);
    await form.locator('.custom-select-trigger').first().click();
    await form.locator('.custom-select-option').filter({ hasText: /^Alibaba ·/ }).click();
    assert.equal(await endpoint.inputValue(), 'https://dashscope.aliyuncs.com/compatible-mode/v1');
    await select('Anthropic');
    assert.equal(await endpoint.inputValue(), 'https://dashscope.aliyuncs.com/apps/anthropic');
    await form.locator('.custom-select-trigger').first().click();
    await form.locator('.custom-select-option').filter({ hasText: 'TaoToken' }).click();
    assert.equal(await endpoint.inputValue(), connections[0].base_urls[0]);
    await endpoint.fill(connections[0].base_urls[2]);
    await select('Anthropic');
    assert.equal(await endpoint.inputValue(), connections[1].base_urls[2]);
    await endpoint.fill('https://custom.example/team');
    await select('OpenAI');
    assert.equal(await endpoint.inputValue(), 'https://custom.example/team');
    await endpoint.fill(connections[0].base_urls[1]);
    await select('Anthropic');
    await form.getByRole('button', { name: 'Fetch available models' }).click();
    const picker = page.locator('.model-picker-modal');
    await picker.getByText('listed-model', { exact: true }).waitFor();
    assert.equal(discoveries.at(-1).type, 'anthropic');
    assert.equal(discoveries.at(-1).base_url, connections[1].base_urls[1]);
    await picker.getByRole('button', { name: 'Cancel', exact: true }).click();
    await model.fill('manual-model');
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), 'no horizontal overflow');
    await page.screenshot({ path: `/tmp/atomcode-provider-protocol-${width}.png` });
    const previous = writes.length;
    await form.locator('.add-model-actions .btn-primary').click();
    await page.waitForFunction(() => !document.querySelector('.add-provider-card'));
    assert.equal(writes.length, previous + 1);
    assert.equal(writes.at(-1).provider, 'taotoken');
    assert.equal(writes.at(-1).protocol, 'anthropic');
    assert.equal(writes.at(-1).base_url, connections[1].base_urls[1]);
    assert.equal(writes.at(-1).models[0].model, 'manual-model');
  }
  assert.deepEqual(errors, []);
  console.log('PASS: vendor protocol entry, plan preservation, custom URLs, discovery/save consistency, desktop/mobile');
} catch (error) {
  await page.screenshot({ path: '/tmp/atomcode-provider-protocol-failure.png' });
  console.error((await page.locator('body').innerText()).slice(-3500)); throw error;
} finally {
  await browser.close(); for (const client of clients) client.end(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
}
