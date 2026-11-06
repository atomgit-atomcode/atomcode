// Real browser components with local fixtures; no daemon or provider calls.
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { chromium } from 'playwright';
import { createServer } from 'vite';
const root = fileURLToPath(new URL('../../', import.meta.url)).replace(/\/$/, '');
const esbuild = createRequire(root + '/extensions/vscode/package.json')('esbuild');
const bundle = await esbuild.build({
  stdin: {
    contents: `
      import React from 'react';
      import { createRoot } from 'react-dom/client';
      import { ChatProvider, useChatContext } from './state/ChatProvider';
      import { MessageList } from './components/MessageList';
      function Probe() {
        window.ctx = useChatContext();
        return React.createElement(MessageList);
      }
      window.acquireVsCodeApi = () => ({ postMessage() {}, getState() { return {}; }, setState() {} });
      createRoot(document.getElementById('root')).render(
        React.createElement(ChatProvider, null, React.createElement(Probe))
      );
    `,
    resolveDir: root + '/extensions/vscode/webview-ui/src',
    loader: 'tsx',
  },
  bundle: true,
  write: false,
  platform: 'browser',
  format: 'iife',
  define: { 'process.env.NODE_ENV': '"production"' },
});
const server = await createServer({ root: root + '/webui', server: { host: '127.0.0.1', port: 0 } });
await server.listen();
const browser = await chromium.launch({ executablePath: process.env.SCROLL_IME_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const results = {};
try {
    const page = await browser.newPage({ viewport: { width: 900, height: 800 } });
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.route('**/__plugin.js', r => r.fulfill({ contentType: 'text/javascript', body: bundle.outputFiles[0].text }));
    await page.route('**/__plugin', r => r.fulfill({ contentType: 'text/html', body: '<!doctype html><div id="root"></div><style>.messages-container{height:500px;overflow:auto;overflow-anchor:none}.user-message{min-height:40px}body{margin:0}</style><script src="/__plugin.js"></script>' }));
    const base = 'http://127.0.0.1:' + server.httpServer.address().port;
    await page.goto(base + '/__plugin');
    await page.waitForFunction(() => window.ctx);
    await page.evaluate(() => {
      for (let i = 0; i < 100; i++) {
        window.ctx.dispatch({ type: 'ADD_USER_MESSAGE', text: 'message ' + i + ' ' + 'content '.repeat(40) });
      }
      window.postMessage({ type: 'generationStarted' }, '*');
    });
    await page.waitForTimeout(150);
    const metrics = () => page.locator('.messages-container').evaluate(e => ({ top: e.scrollTop, gap: e.scrollHeight - e.clientHeight - e.scrollTop, height: e.scrollHeight }));
    await page.locator('.messages-container').evaluate(e => e.scrollTop = e.scrollHeight);
    await page.waitForTimeout(60);
    await page.locator('.messages-container').hover();
    await page.mouse.wheel(0, -25);
    await page.waitForTimeout(60);
    const before = await metrics();
    await page.evaluate(() => window.postMessage({ type: 'text', content: 'stream update\n' }, '*'));
    await page.waitForTimeout(80);
    const after = await metrics();
    results.smallScroll = { before, after };
    assert.ok(before.gap > 0);
    assert.equal(after.top, before.top, 'small upward scrolling must survive the next stream update');
    await page.mouse.wheel(0, -250);
    await page.waitForTimeout(60);
    const largeBefore = await metrics();
    await page.evaluate(() => window.postMessage({ type: 'text', content: 'another update\n' }, '*'));
    await page.waitForTimeout(80);
    results.largeScroll = { before: largeBefore, after: await metrics() };
    assert.equal(results.largeScroll.after.top, largeBefore.top, 'large upward scrolling also remains stable');
    await page.evaluate(() => window.ctx.dispatch({ type: 'TOGGLE_SEARCH' }));
    await page.locator('.search-bar-input').waitFor();
    await page.locator('.search-bar-input').dispatchEvent('keydown', { key: 'Escape', keyCode: 229, isComposing: true });
    await page.waitForTimeout(40);
    results.imeComposingPreservesSearch = await page.locator('.search-bar-input').count() === 1;
    assert.ok(results.imeComposingPreservesSearch);
    await page.locator('.search-bar-input').dispatchEvent('keydown', { key: 'Escape', keyCode: 229, isComposing: false });
    await page.waitForTimeout(40);
    assert.equal(await page.locator('.search-bar-input').count(), 1, '229 Escape must not close search');
    await page.locator('.search-bar-input').press('Escape');
    assert.equal(await page.locator('.search-bar-input').count(), 0, 'ordinary Escape still closes search');
    await page.locator('.messages-container').evaluate(e => e.scrollTop = e.scrollHeight);
    await page.waitForTimeout(60);
    await page.evaluate(() => window.postMessage({ type: 'text', content: '\nresumed output\n' }, '*'));
    await page.waitForTimeout(80);
    assert.equal((await metrics()).gap, 0, 'reaching the bottom restores following');
    // The explicit return control must restore follow too.
    await page.mouse.wheel(0, -200);
    await page.locator('.scroll-to-bottom-btn').waitFor();
    await page.locator('.scroll-to-bottom-btn').click();
    await page.waitForFunction(() => {
      const e = document.querySelector('.messages-container');
      return e.scrollHeight - e.clientHeight - e.scrollTop <= 2;
    });
    await page.evaluate(() => window.postMessage({ type: 'text', content: '\nlatest button output\n' }, '*'));
    await page.waitForTimeout(80);
    assert.equal((await metrics()).gap, 0);
    results.errors = errors;
    assert.deepEqual(errors, []);
    await page.route('**/__rename', async (r) => r.fulfill({ contentType: 'text/html', body: await server.transformIndexHtml('/__rename', `<!doctype html><div id="root"></div><script type="module">import{h,render}from'preact';import{SettingsProvider}from'/src/settings.tsx';import{RenameDialog}from'/src/components/SessionDialogs.tsx';window.saved=0;render(h(SettingsProvider,null,h(RenameDialog,{session:{id:'fixture',project_hash:'fixture',name:'old'},onClose(){},onDone(){window.saved++}})),document.getElementById('root'));</script>`) }));
    const renames = [];
    await page.route('**/projects/fixture/sessions/fixture/rename', async (r) => { renames.push({ method: r.request().method(), body: r.request().postData() }); await r.fulfill({ contentType: 'application/json', body: '{"success":true}' }); });
    await page.goto(base + '/__rename');
    await page.locator('.menu-input').fill('new name');
    await page.locator('.menu-input').dispatchEvent('keydown', { key: 'Enter', keyCode: 229, isComposing: true });
    await page.waitForTimeout(80);
    results.renameNormalCompositionRequests = renames.length;
    assert.equal(renames.length, 0);
    await page.locator('.menu-input').dispatchEvent('keydown', { key: 'Enter', keyCode: 229, isComposing: false });
    await page.waitForTimeout(150);
    results.rename229Requests = renames.slice();
    assert.equal(renames.length, 0, '229 Enter must not submit rename');
    await page.locator('.menu-input').press('Enter');
    await page.waitForFunction(() => window.saved === 1);
    results.renameSaved = await page.evaluate(() => window.saved);
    assert.equal(renames.length, 1);
    console.log(JSON.stringify(results, null, 2));
    console.log('Scroll following and secondary-input IME browser checks passed.');
}
finally {
    await browser.close();
    await server.close();
}
