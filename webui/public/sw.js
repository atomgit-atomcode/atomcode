// What the installed app (PWA) shows when the webui is not running.
//
// Without this, opening the app with nothing serving 127.0.0.1:13457 showed
// the browser's own "can't reach this page", which says nothing about how to
// start it. This worker answers a page load that cannot reach the server with
// a page that does, and does nothing else:
//
// - it caches nothing — every request goes to the server as before, so an
//   upgraded webui is never served from an old copy;
// - it only touches page loads (navigations); API calls, assets and the live
//   stream are left to the browser.

const OFFLINE = {
  zh: {
    lang: 'zh-CN',
    title: 'AtomCode webui 没在运行',
    lead: '这个页面由 AtomCode 在本机提供，现在没有在运行。用下面任一方式启动，然后用它打印的链接打开：',
    standalone: '在终端运行 <code>atomcode webui</code>，独立运行，关闭这个终端窗口才会停止；',
    inSession: '或在 AtomCode 里输入 <code>/webui</code>，随这个 AtomCode 退出而停止。',
    note: '每次启动的链接都不同（带有新的访问令牌），请用终端打印的那条打开。',
    retry: '已经启动了，重试',
  },
  en: {
    lang: 'en',
    title: 'AtomCode webui is not running',
    lead: 'This page is served by AtomCode on this machine, and it is not running. Start it either way below, then open the link it prints:',
    standalone: 'run <code>atomcode webui</code> in a terminal — it keeps running until that terminal window is closed;',
    inSession: 'or type <code>/webui</code> in AtomCode — it stops when that AtomCode exits.',
    note: 'Each start prints a different link (it carries a new access token); open the one the terminal printed.',
    retry: 'It is running now — try again',
  },
};

function offlinePage(acceptLanguage) {
  const t = /^zh/i.test(acceptLanguage || '') ? OFFLINE.zh : OFFLINE.en;
  return `<!doctype html>
<html lang="${t.lang}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>${t.title}</title>
<style>
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
         font-family: system-ui, -apple-system, "Segoe UI", sans-serif; background: #f7f7f5; color: #222; }
  @media (prefers-color-scheme: dark) { body { background: #1e1e1e; color: #ddd; } .card { background: #262626; border-color: #3a3a3a; } }
  .card { max-width: 560px; margin: 24px; padding: 28px 32px; background: #fff; border: 1px solid #ddd; border-radius: 10px; }
  h1 { font-size: 1.25rem; margin: 0 0 12px; }
  p, li { line-height: 1.65; }
  code { padding: 1px 6px; border-radius: 4px; background: rgba(128,128,128,0.18); }
  .note { opacity: 0.75; font-size: 0.9rem; }
  button { margin-top: 8px; padding: 8px 16px; border-radius: 6px; border: 1px solid #c2410c; background: #c2410c; color: #fff; cursor: pointer; }
</style></head>
<body><div class="card">
  <h1>${t.title}</h1>
  <p>${t.lead}</p>
  <ul><li>${t.standalone}</li><li>${t.inSession}</li></ul>
  <p class="note">${t.note}</p>
  <button onclick="location.reload()">${t.retry}</button>
</div></body></html>`;
}

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('fetch', (event) => {
  if (event.request.mode !== 'navigate') return;
  event.respondWith(
    fetch(event.request).catch(
      () =>
        new Response(offlinePage(event.request.headers.get('Accept-Language')), {
          status: 503,
          headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' },
        }),
    ),
  );
});
