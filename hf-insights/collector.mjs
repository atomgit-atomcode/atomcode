import { setTimeout as sleep } from 'node:timers/promises';
import { ProxyAgent } from 'undici';

const proxyUrl = process.env.HF_INSIGHTS_PROXY || process.env.HTTPS_PROXY || process.env.https_proxy || process.env.HTTP_PROXY || process.env.http_proxy;
const dispatcher = proxyUrl ? new ProxyAgent(proxyUrl) : undefined;

export async function requestPage(url, { token, fetcher = fetch, wait = sleep } = {}) {
  for (let attempt = 0; ; attempt++) {
    let response;
    try {
      response = await fetcher(url, { headers: token ? {Authorization:`Bearer ${token}`} : {}, signal: AbortSignal.timeout(60000), redirect:'error', ...(dispatcher ? {dispatcher} : {}) });
      if (response.ok) {
        const body = await response.text();
        return new Response(body, {status: response.status, headers: response.headers});
      }
      if (response.status !== 429 && response.status < 500) throw new Error(`Permanent HTTP ${response.status}`);
    } catch (error) {
      if (attempt >= 5 || error.message.startsWith('Permanent')) throw error;
      const smaller = new URL(url);
      if (Number(smaller.searchParams.get('limit')) > 100) {
        smaller.searchParams.set('limit', '100');
        url = smaller;
      }
    }
    if (attempt >= 5) throw new Error(`Retry exhausted: HTTP ${response?.status}`);
    const retry = response?.headers.get('retry-after');
    const delay = retry ? (/^\d+$/.test(retry) ? Number(retry)*1000 : Date.parse(retry)-Date.now()) : 1000*2**attempt;
    await wait(Math.max(1000, Math.min(Number.isFinite(delay)?delay:1000, 3600000)));
  }
}

export async function scanCatalog(store, kind, endpoint, {max = 0, pageSize = 100, delay = 700, request = requestPage, token} = {}) {
  const started = Date.now();
  const initial = new URL(endpoint);
  initial.searchParams.set('limit', String(pageSize));
  initial.searchParams.set('full', 'true');
  const run = store.begin(kind, initial.href);
  let cursor = run.cursor;
  let pages = run.pages;
  try {
    if (max > 0 && store.count(run.id) >= max && run.status === 'paused') {
      return {run_id:run.id, kind, pages, complete:false, stop_reason:'configured_budget', duration_ms:0, count:store.count(run.id), unchanged:true};
    }
    while (cursor) {
      const url = new URL(cursor);
      if (url.origin !== initial.origin) throw new Error('Cross-origin cursor refused');
      const requestedSize = Number(process.env.HF_INSIGHTS_REQUEST_PAGE_SIZE || pageSize);
      if (!Number.isInteger(requestedSize) || requestedSize < 1 || requestedSize > 1000) throw new Error('Request page size must be between 1 and 1000');
      url.searchParams.set('limit', String(requestedSize));
      if (process.env.HF_INSIGHTS_COMPACT === '1') url.searchParams.delete('full');
      const response = await request(url, {token});
      const rows = await response.json();
      if (!Array.isArray(rows)) throw new Error('Expected repository array');
      const next = response.headers.get('link')?.match(/<([^>]+)>;\s*rel="?next"?/i)?.[1];
      const nextUrl = next ? new URL(next, initial).href : null;
      if (nextUrl && new URL(nextUrl).origin !== initial.origin) throw new Error('Cross-origin cursor refused');
      if (nextUrl === cursor) throw new Error('Repeated cursor');
      const remaining = max > 0 ? Math.max(0, max - store.count(run.id)) : rows.length;
      const validRows = rows.filter((row) => {
        if (!row || typeof row !== 'object' || typeof row.id !== 'string' || !row.id.trim()) return false;
        for (const key of ['downloads', 'likes']) {
          if (row[key] != null && (!Number.isFinite(Number(row[key])) || Number(row[key]) < 0)) return false;
        }
        return true;
      });
      const pageRows = validRows;
      const rejected = rows.length - validRows.length;
      const paused = Boolean(nextUrl && max > 0 && store.count(run.id) + pageRows.length >= max);
      const commitCursor = nextUrl;
      store.page(run.id, pageRows, commitCursor, commitCursor ? (paused ? 'paused' : 'running') : 'complete', rejected);
      pages++;
      cursor = commitCursor;
      if (paused || (max > 0 && store.count(run.id) >= max)) break;
      if (cursor) await sleep(delay);
    }
    return {run_id:run.id, kind, pages, complete:!cursor, stop_reason:cursor?'configured_budget':'end_of_catalog', duration_ms:Date.now()-started, total_elapsed_ms:Date.now()-run.started, count:store.count(run.id)};
  } catch (error) { store.fail(run.id,error); throw error; }
}
