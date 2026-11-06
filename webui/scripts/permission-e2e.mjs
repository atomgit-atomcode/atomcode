// Real approval component and HTTP payloads; no daemon, provider or project writes.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';

const server = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), server: { host: '127.0.0.1', port: 0 } });
let browser;
try {
  await server.listen();
  browser = await chromium.launch({ executablePath: process.env.PERMISSION_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
  const page = await browser.newPage({ viewport: { width: 900, height: 850 } });
  const errors = [];
  const submitted = [];
  let accepted = false;
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/chat/permission', async (route) => {
    submitted.push(route.request().postDataJSON());
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify({ success: accepted }) });
  });
  await page.route('**/__permission-test', async (route) => route.fulfill({ contentType: 'text/html', body: await server.transformIndexHtml('/__permission-test', `<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head><body><div id="root" style="max-width:760px;margin:24px auto;padding:12px"></div><script type="module">
    import { h, render } from 'preact';
    import { SettingsProvider } from '/src/settings.tsx';
    import { PermissionCard } from '/src/components/PermissionCard.tsx';
    import { resolvePendingAfterDecision } from '/src/lib/pendingPermission.ts';
    import '/src/styles/theme.css';import '/src/styles/app.css';
    localStorage.setItem('atomcode.lang', 'en');
    window.resolved=[];
    const draw = () => { const req=window.current; render(h(SettingsProvider, null, req && h(PermissionCard, { key:req.call_id, req, onDone:()=>{window.resolved.push(req.call_id);window.current=resolvePendingAfterDecision(window.current,req.call_id);draw();} })),document.getElementById('root')); };
    window.mountPermission=(req)=>{window.current=req;draw();};
  </script></body></html>`) }));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/__permission-test`);
  await page.waitForFunction(() => typeof window.mountPermission === 'function');
  const request = { session_id: 'session', tool_name: 'bash', call_id: 'call-1', arguments: { command: 'rm -rf scratch' }, reason: 'Recursive removal requires approval. No allow rule covers rm -rf scratch.', project_rule: { rule: 'Bash(rm -rf scratch)', file: '/workspace/.atomcode/settings.local.json' } };
  await page.evaluate((req) => window.mountPermission(req), request);
  await page.getByText(request.reason, { exact: true }).waitFor();
  await page.getByText('Matches this exact command; saved rules apply after reload or in a new session.', { exact: true }).waitFor();
  await page.screenshot({ path: '/tmp/atomcode-project-approval-wide.png', fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: '/tmp/atomcode-project-approval-narrow.png', fullPage: true });
  const save = page.getByRole('button', { name: 'Approve and save exact project rule', exact: true });
  const box = await save.boundingBox();
  assert.ok(box && box.x >= 0 && box.x + box.width <= 390);
  await save.click();
  await page.getByText(/This approval request is no longer pending/).waitFor();
  assert.deepEqual(await page.evaluate(() => window.resolved), [], 'rejected delivery keeps the card');
  assert.deepEqual(submitted[0], { session_id: 'session', decision: 'allow_project', tool_name: 'bash', call_id: 'call-1' });
  assert.equal('rule' in submitted[0], false, 'the client cannot choose the saved rule');
  accepted = true;
  await save.click();
  await page.waitForFunction(() => window.resolved.length === 1);
  await page.evaluate((req) => window.mountPermission({ ...req, call_id: 'call-2', project_rule: undefined }), request);
  assert.equal(await page.getByRole('button', { name: 'Approve and save exact project rule', exact: true }).count(), 0, 'unoffered persistence is hidden');
  assert.deepEqual(errors, []);
  console.log('Approval browser checks passed (reason, rule preview, exact call correlation, rejected delivery and retry).');
} finally {
  await browser?.close();
  await server.close();
}
