import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import { getProxyDispatcher } from './proxy/proxy-agent.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SNAPSHOT_PATH = join(__dirname, 'data', 'icons-manifest.v1.json');
const MANIFEST_URL = 'https://open.huaweicloud.com/openplatform/icons/manifest.v1.json';
const ICONS_PAGE_URL = 'https://open.huaweicloud.com/openplatform/icons.html';
const HTTP_TIMEOUT_MS = 10000;
const MAX_RESULTS = 5;

let cachedManifest = null;

export function clearIconCache() {
  cachedManifest = null;
}

async function fetchLiveManifest() {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HTTP_TIMEOUT_MS);
  try {
    const dispatcher = await getProxyDispatcher(MANIFEST_URL);
    const fetchOpts = {
      headers: { 'User-Agent': 'huaweicloud-devkit/1.0' },
      signal: controller.signal,
    };
    if (dispatcher) fetchOpts.dispatcher = dispatcher;
    const resp = await fetch(MANIFEST_URL, fetchOpts);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const data = await resp.json();
    if (!data || !Array.isArray(data.icons)) {
      throw new Error('Manifest is missing the icons array.');
    }
    return data;
  } finally {
    clearTimeout(timer);
  }
}

function loadSnapshot() {
  const manifest = JSON.parse(readFileSync(SNAPSHOT_PATH, 'utf8'));
  if (!manifest || !Array.isArray(manifest.icons)) {
    throw new Error('Bundled icons manifest is corrupted.');
  }
  return manifest;
}

async function loadManifest() {
  if (cachedManifest) return cachedManifest;
  if (process.env.HUAWEICLOUD_ICONS_OFFLINE === '1') {
    cachedManifest = { manifest: loadSnapshot(), source: 'snapshot' };
    return cachedManifest;
  }
  try {
    const live = await fetchLiveManifest();
    cachedManifest = { manifest: live, source: 'live' };
  } catch {
    cachedManifest = { manifest: loadSnapshot(), source: 'snapshot' };
  }
  return cachedManifest;
}

function normalizeToken(token) {
  return token.toLowerCase();
}

function scoreIcon(icon, tokens, index) {
  let total = 0;
  const matched = [];
  for (const token of tokens) {
    let best = 0;
    const id = (icon.id || '').toLowerCase();
    const name = (icon.name || '').toLowerCase();
    if (id === token || (index.aliases[token] || []).includes(id)) {
      best = 10;
    } else if (name === token) {
      best = 9;
    } else if (name.startsWith(token)) {
      best = 8;
    } else if (name.includes(token)) {
      best = 7;
    } else if (id.includes(token)) {
      best = 5;
    } else if ((index.aliases[id] || []).some((a) => a.includes(token))) {
      best = 6;
    } else if ((index.tags[id] || []).some((t) => t.includes(token))) {
      best = 4;
    } else if ((index.categories[id] || '').includes(token)) {
      best = 3;
    } else if ((index.descriptions[id] || '').includes(token)) {
      best = 2;
    }
    if (best > 0) {
      total += best;
      matched.push(token);
    }
  }
  return [total, matched];
}

export async function getServiceIcon(service = '', category = '') {
  const query = String(service || '').trim();
  const catFilter = String(category || '')
    .trim()
    .toLowerCase();
  if (!query && !catFilter) {
    return {
      ok: false,
      error:
        'service or category is required. Examples: ecs, obs, modelarts, 对象存储, 虚拟私有云, or a category such as 计算 / 存储 / 人工智能.',
      iconsPageUrl: ICONS_PAGE_URL,
    };
  }
  const { manifest, source } = await loadManifest();
  const tokens = query
    .replace(/[,;]/g, ' ')
    .split(/\s+/)
    .filter(Boolean)
    .map((t) => normalizeToken(t));

  const index = { aliases: {}, tags: {}, categories: {}, descriptions: {} };
  for (const icon of manifest.icons) {
    const id = (icon.id || '').toLowerCase();
    index.aliases[id] = (icon.aliases || []).map((a) => String(a).toLowerCase());
    index.tags[id] = (icon.tags || []).map((t) => String(t).toLowerCase());
    index.categories[id] = [
      String(icon.category || '').toLowerCase(),
      String(icon.subcategory || '').toLowerCase(),
    ].join(' ');
    index.descriptions[id] = String(icon.description || '').toLowerCase();
  }

  const results = [];
  for (const icon of manifest.icons) {
    const categoryMatches = !catFilter || String(icon.category || '').toLowerCase() === catFilter;
    if (!categoryMatches) continue;
    const [score, matched] = tokens.length ? scoreIcon(icon, tokens, index) : [0, []];
    if (tokens.length && score === 0) continue;
    results.push({
      id: icon.id,
      name: icon.name,
      category: icon.category,
      subcategory: icon.subcategory || undefined,
      description: icon.description || undefined,
      aliases: icon.aliases,
      product_url: icon.product_url,
      logo: {
        source_url: icon.logo?.source_url,
        local_path: icon.logo?.local_path,
      },
      architecture: icon.architecture
        ? { status: icon.architecture.status, local_path: icon.architecture.local_path }
        : undefined,
      score,
      matched,
    });
  }
  results.sort((a, b) => b.score - a.score);

  return {
    ok: true,
    query,
    category: catFilter || undefined,
    source,
    manifestGeneratedAt: manifest.generated_at,
    count: results.length,
    iconsPageUrl: ICONS_PAGE_URL,
    note:
      source === 'live'
        ? 'Live manifest from open.huaweicloud.com.'
        : 'Live manifest unreachable; using bundled snapshot. Upgrade the package to refresh the bundled snapshot.',
    results: results.slice(0, MAX_RESULTS),
  };
}
