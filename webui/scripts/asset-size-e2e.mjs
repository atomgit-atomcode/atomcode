// Built assets and font rendering over localhost, with no provider or external font service.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile, readdir } from 'node:fs/promises';
import { resolve, extname, relative } from 'node:path';
import { chromium } from 'playwright';
import katex from 'katex';

const root = resolve(import.meta.dirname, '../dist');
const assets = await readdir(resolve(root, 'assets'));
assert(assets.some(name => name.endsWith('.woff2')));
assert(!assets.some(name => /\.(?:woff|ttf)$/.test(name)), 'legacy font fallbacks must not be emitted');
const cssFiles = assets.filter(name => name.endsWith('.css'));
for (const name of cssFiles) {
  const css = await readFile(resolve(root, 'assets', name), 'utf8');
  for (const match of css.matchAll(/url\(["']?([^)'"\s]+)["']?\)/g)) {
    if (match[1].startsWith('data:')) continue;
    const file = resolve(root, 'assets', match[1].replace(/^\/assets\//, ''));
    assert(!relative(root, file).startsWith('..'));
    const bytes = await readFile(file);
    if (file.endsWith('.woff2')) assert.equal(bytes.subarray(0, 4).toString(), 'wOF2');
  }
}
const failures = [];
const server = createServer(async (req, res) => {
  const file = resolve(root, '.' + new URL(req.url, 'http://localhost').pathname);
  if (relative(root, file).startsWith('..')) { res.writeHead(404); res.end(); return; }
  try {
    const data = await readFile(file);
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Content-Type', {'.css':'text/css','.woff2':'font/woff2','.js':'application/javascript'}[extname(file)] ?? 'application/octet-stream');
    res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(done => server.listen(0, '127.0.0.1', done));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({headless:true, executablePath:process.env.ASSET_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'});
try {
  const page = await browser.newPage();
  page.on('requestfailed', req => failures.push(req.url()));
  page.on('response', response => { if(response.status() >= 400) failures.push(response.url()); });
  const math = katex.renderToString(String.raw`\frac{a}{b}+\sum_{n=0}^{\infty}x_n+\mathbb{R}+\mathcal{F}`, {displayMode:true});
  await page.setContent(cssFiles.map(name => `<link rel="stylesheet" href="${base}/assets/${name}">`).join('') + `<div id="math">${math}</div>`);
  const loaded = await page.evaluate(async () => {
    await document.fonts.ready;
    const faces = await document.fonts.load('20px KaTeX_Main');
    return faces.length > 0 && faces.every(face => face.status === 'loaded');
  });
  assert(loaded, 'local KaTeX WOFF2 faces load');
  assert((await page.locator('#math .katex-html').boundingBox()).width > 0);
  assert.deepEqual(failures, [], 'every font resource is available offline from the local server');
  console.log(`asset packaging: ${assets.length} files, WOFF2-only, all CSS URLs resolve, local math rendering passed`);
} finally {
  await browser.close();
  await new Promise(done => server.close(done));
}
