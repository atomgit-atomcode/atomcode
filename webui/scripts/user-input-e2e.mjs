// Real component/browser interactions; request transport is captured locally.
// Run with: npm run test:user-input. No daemon or model calls are made.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';

const server = await createServer({
  root: fileURLToPath(new URL('..', import.meta.url)),
  server: { host: '127.0.0.1', port: 0 },
});
let browser;
try {
  await server.listen();
  const address = server.httpServer.address();
  browser = await chromium.launch({
    executablePath: process.env.USER_INPUT_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    headless: true,
  });
  const page = await browser.newPage({ viewport: { width: 900, height: 800 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/__question-edit-test', async (route) => route.fulfill({
    contentType: 'text/html',
    body: await server.transformIndexHtml('/__question-edit-test', `<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head>
      <body><div id="root" style="max-width:760px;margin:24px auto;padding:12px"></div>
      <script type="module">
        import { h, render } from 'preact';
        import { SettingsProvider } from '/src/settings.tsx';
        import { UserInputCard } from '/src/components/UserInputCard.tsx';
        import '/src/styles/theme.css';
        import '/src/styles/app.css';
        localStorage.setItem('atomcode.lang', 'en');
        window.answers = [];
        window.done = 0;
        window.accepted = true;
        window.mountQuestion = (req) => render(h(SettingsProvider, null, req && h(UserInputCard, {
          req,
          onDone: () => { window.done++; },
          submitAnswer: async (body) => { window.answers.push(body); return { accepted: window.accepted }; },
        })), document.getElementById('root'));
      </script></body></html>`),
  }));
  await page.goto(`http://127.0.0.1:${address.port}/__question-edit-test`);
  await page.waitForFunction(() => typeof window.mountQuestion === 'function');
  const base = {
    type: 'user_input_request', request_id: 1, header: 'Implementation plan',
    question: 'Which approach should we use?', mode: 'single',
    options: [{ label: '用方案 A', description: 'Start with a small, reviewable change.' }, { label: '用方案 B' }, { label: 'C' }],
  };
  const mount = (req) => page.evaluate((value) => window.mountQuestion(value), req);
  const edit = async (label, text) => {
    await page.getByRole('button', { name: `Edit option: ${label}`, exact: true }).click();
    const field = page.getByRole('textbox', { name: `Edit option: ${label}`, exact: true });
    await page.waitForFunction((el) => document.activeElement === el, await field.elementHandle());
    await field.fill(text);
    return field;
  };
  await mount(base);
  let field = await edit('用方案 A', '用方案 A，但先只做第 1 步');
  await field.dispatchEvent('keydown', { key: 'Enter', isComposing: true });
  assert.equal(await page.evaluate(() => window.answers.length), 0, 'IME confirmation is not submission');
  await page.evaluate(() => { window.accepted = false; });
  await field.press('Enter');
  await page.getByText('Submit failed, please retry').waitFor();
  assert.equal(await page.evaluate(() => window.done), 0);
  assert.deepEqual(await page.evaluate(() => window.answers[0]), {
    request_id: 1, declined: false, selected: [], text: '用方案 A，但先只做第 1 步',
  });
  await page.evaluate(() => { window.accepted = true; });
  await page.getByRole('button', { name: 'Submit', exact: true }).click();
  await page.waitForFunction(() => window.done === 1);

  await mount({ ...base, request_id: 2, mode: 'multiple' });
  field = await edit('用方案 A', 'A 第一阶段');
  await field.press('Enter');
  field = await edit('用方案 B', 'B 第二阶段');
  await field.press('Enter');
  await page.getByRole('checkbox', { name: 'C', exact: true }).check();
  await page.getByRole('checkbox', { name: 'Other', exact: true }).check();
  await page.getByRole('textbox', { name: 'Other', exact: true }).fill('保留独立说明');
  field = await edit('用方案 A', '');
  await field.press('Enter');
  assert.equal(await page.locator('.user-input-option-editor').count(), 1, 'blank edit remains open');
  await field.press('Escape');
  assert.equal(await page.getByRole('checkbox', { name: /A 第一阶段/ }).isChecked(), true);
  await page.getByRole('button', { name: 'Submit', exact: true }).click();
  await page.waitForFunction(() => window.done === 2);
  assert.deepEqual(await page.evaluate(() => window.answers.at(-1)), {
    request_id: 2, declined: false, selected: ['C'], text: 'A 第一阶段\nB 第二阶段\n保留独立说明',
  });

  const questions = [{ ...base, mode: 'multiple' }, { header: 'Name', question: 'Project name?', mode: 'text', options: [] }];
  await mount({ ...base, request_id: 3, questions });
  await (await edit('用方案 A', 'A batch')).press('Enter');
  await (await edit('用方案 B', 'B batch')).press('Enter');
  await page.getByRole('button', { name: '→', exact: true }).click();
  await page.locator('textarea').fill('Example project');
  await page.getByRole('button', { name: '←', exact: true }).click();
  assert.equal(await page.getByRole('checkbox', { name: /A batch/ }).isChecked(), true);
  assert.equal(await page.getByRole('checkbox', { name: 'B batch', exact: true }).isChecked(), true);
  field = await edit('用方案 A', 'A batch revision');
  await page.screenshot({ path: '/tmp/atomcode-question-edit-wide.png', fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  const box = await field.boundingBox();
  assert.ok(box && box.x >= 0 && box.x + box.width <= 390, 'editor fits a narrow viewport');
  await page.screenshot({ path: '/tmp/atomcode-question-edit-narrow.png', fullPage: true });
  await field.press('Escape');
  await page.getByRole('button', { name: '→', exact: true }).click();
  assert.equal(await page.locator('textarea').inputValue(), 'Example project');
  await page.getByRole('button', { name: 'Submit', exact: true }).click();
  await page.waitForFunction(() => window.done === 3);
  assert.deepEqual(await page.evaluate(() => window.answers.at(-1).responses), [
    { declined: false, selected: [], text: 'A batch\nB batch' },
    { declined: false, selected: [], text: 'Example project' },
  ]);

  await mount({ ...base, request_id: 4 });
  await edit('用方案 A', 'stale draft');
  const sent = await page.evaluate(() => window.answers.length);
  await mount({ ...base, request_id: 5 });
  assert.equal(await page.locator('.user-input-option-editor').count(), 0, 'replacement request has no old draft');
  await edit('用方案 A', 'withdrawn draft');
  await mount(null);
  assert.equal(await page.locator('.user-input-option-editor').count(), 0);
  assert.equal(await page.evaluate(() => window.answers.length), sent, 'withdrawal does not submit');
  assert.deepEqual(errors, []);
  console.log('Question editing browser checks passed (single, multiple, batch, IME, retry, replacement and withdrawal).');
} finally {
  await browser?.close();
  await server.close();
}
