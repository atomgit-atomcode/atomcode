import puppeteer from 'puppeteer';
import { fileURLToPath } from 'url';
import { dirname, resolve } from 'path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const htmlPath = resolve(__dirname, 'index.html');
const pdfPath = resolve(__dirname, 'atomcode-launch.pdf');
const pngPath = resolve(__dirname, 'atomcode-launch.png');

const browser = await puppeteer.launch({ headless: 'new' });
const page = await browser.newPage();

await page.setViewport({ width: 1440, height: 900, deviceScaleFactor: 2 });

// Use a local HTTP server so CDN scripts load properly (file:// can block external JS)
const http = await import('http');
const fs = await import('fs');
const server = http.createServer((req, res) => {
  const content = fs.readFileSync(htmlPath, 'utf-8');
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(content);
});

await new Promise(r => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

await page.goto(`http://127.0.0.1:${port}`, { waitUntil: 'networkidle0', timeout: 30000 });

// Wait for Tailwind CDN to finish compiling
await page.waitForFunction(() => {
  // Check if Tailwind has injected styles by testing a known class
  const el = document.querySelector('.bg-surface-950');
  if (!el) return false;
  const style = getComputedStyle(el);
  return style.backgroundColor !== 'rgba(0, 0, 0, 0)' && style.backgroundColor !== '';
}, { timeout: 15000 });

await page.waitForFunction(() => document.fonts.ready, { timeout: 10000 });
await new Promise(r => setTimeout(r, 3000));

// Force screen media
await page.emulateMediaType('screen');

// Full page screenshot first (guaranteed 100% faithful)
await page.screenshot({ path: pngPath, fullPage: true, type: 'png' });
console.log(`PNG: ${pngPath}`);

// PDF: embed the pixel-perfect screenshot into a PDF page
// This guarantees 100% fidelity — the PNG IS the source of truth.
const imgBuf = fs.readFileSync(pngPath);
const imgBase64 = imgBuf.toString('base64');
const imgWidth = 1440;
const bodyHeight = await page.evaluate(() => document.body.scrollHeight);

const pdfPage = await browser.newPage();
await pdfPage.setContent(`
  <html><head><style>
    * { margin: 0; padding: 0; }
    body { width: ${imgWidth}px; }
    img { width: 100%; display: block; }
  </style></head>
  <body><img src="data:image/png;base64,${imgBase64}"></body></html>
`, { waitUntil: 'load' });

await pdfPage.pdf({
  path: pdfPath,
  width: `${imgWidth}px`,
  height: `${bodyHeight}px`,
  printBackground: true,
  margin: { top: 0, right: 0, bottom: 0, left: 0 },
});
console.log(`PDF: ${pdfPath} (${bodyHeight}px)`);

server.close();
await browser.close();
