import { createServer } from 'node:http';
import { mkdir, readFile } from 'node:fs/promises';
import { mkdirSync } from 'node:fs';
import { Store } from './store.mjs';
import { scanCatalog } from './collector.mjs';
import { FAMILY_METADATA } from './family-metadata.mjs';

import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('.', import.meta.url));
const PORT = Number(process.env.HF_INSIGHTS_PORT || 8787);
const HOST = process.env.HF_INSIGHTS_HOST || '127.0.0.1';
const DATA_DIR = process.env.HF_INSIGHTS_DATA_DIR || join(ROOT, 'data');
mkdirSync(DATA_DIR, { recursive: true });
const DB_PATH = process.env.HF_INSIGHTS_DB || join(DATA_DIR, 'hf-insights.sqlite');
const store = new Store(DB_PATH);
const PAGE_SIZE = Math.min(Math.max(Number(process.env.HF_INSIGHTS_PAGE_SIZE || 100), 10), 100);
const MAX_MODELS = Math.max(Number(process.env.HF_INSIGHTS_MAX_MODELS ?? 0), 0);
const REQUEST_DELAY_MS = Math.max(Number(process.env.HF_INSIGHTS_REQUEST_DELAY_MS || 700), 0);
const REFRESH_MS = process.env.HF_INSIGHTS_REFRESH_SECONDS
  ? Math.max(Number(process.env.HF_INSIGHTS_REFRESH_SECONDS), 1) * 1000
  : Math.max(Number(process.env.HF_INSIGHTS_REFRESH_HOURS || 6), 1) * 3600_000;
const REFRESH_INTERVAL_LABEL = process.env.HF_INSIGHTS_REFRESH_SECONDS
  ? `${REFRESH_MS / 1000}s`
  : `${REFRESH_MS / 3600_000}h`;
const HF_URL = process.env.HF_INSIGHTS_API_URL || 'https://huggingface.co/api/models';
const DYNAMIC_MIN_MODELS = Math.max(Number(process.env.HF_INSIGHTS_DYNAMIC_MIN_MODELS || 25), 1);
const DYNAMIC_MIN_DOWNLOADS = Math.max(Number(process.env.HF_INSIGHTS_DYNAMIC_MIN_DOWNLOADS || 500000), 0);
const DYNAMIC_NAME_ALIASES = { 'stepfun-ai':'StepFun', 'minimaxai':'MiniMax', 'allenai':'AI2', 'nousresearch':'NousResearch', 'cohereforai':'Cohere' };
const FAMILY_RULES = [
  ['deepseek', 'DeepSeek', '推理、代码与 MoE'], ['qwen', 'Qwen', '通用、多语言与代码'],
  ['glm|chatglm', 'GLM', '双语通用与对话'], ['llama', 'Llama', '开放基础模型生态'],
  ['mistral|mixtral', 'Mistral', '高效与 MoE'], ['gemma', 'Gemma', '轻量开放模型'],
  ['phi', 'Phi', '小型高效模型'], ['yi-', 'Yi', '中英双语模型'],
  ['internlm', 'InternLM', '中文与工具调用'], ['baichuan', 'Baichuan', '中文通用模型'],
  ['stable-diffusion|flux|sdxl', '视觉生成', '图像生成与扩散模型'], ['whisper', 'Whisper', '语音识别'],
  ['bert|roberta|electra', '语言理解', '编码器与嵌入模型']
];
const savedReport = store.latestReport();
let current = savedReport?.families?.every(f => Object.hasOwn(f, 'country')) ? savedReport : null;
let refreshPromise = null;
let nextRefreshAt = Date.now() + REFRESH_MS;
let automaticRefreshes = 0;

const json = (res, code, body) => { res.writeHead(code, {'content-type':'application/json; charset=utf-8','cache-control':'no-store'}); res.end(JSON.stringify(body)); };
const tokenHeaders = process.env.HF_TOKEN ? { authorization: `Bearer ${process.env.HF_TOKEN}` } : {};
const slug = name => ({'视觉生成':'vision','语言理解':'encoders'}[name] || name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, ''));
const basesFor = model => {
  const card = model.cardData?.base_model;
  const cardBases = (Array.isArray(card) ? card : [card]).filter(x => typeof x === 'string');
  const tagBases = (Array.isArray(model.tags) ? model.tags : [])
    .filter(tag => typeof tag === 'string' && tag.startsWith('base_model:'))
    .map(tag => tag.slice('base_model:'.length).replace(/^(adapter|finetune|quantized|merged):/, ''))
    .filter(Boolean);
  return [...new Set([...cardBases, ...tagBases])];
};
const namespaceFor = model => {
  const bases = basesFor(model);
  const source = bases[0] || model.author || String(model.id || '').split('/')[0] || 'unknown';
  return source.split('/')[0];
};

const explicitFamilyFor = model => {
  const hay = `${model.id} ${basesFor(model).join(' ')}`.toLowerCase();
  const hit = FAMILY_RULES.find(([pattern]) => new RegExp(pattern).test(hay));
  if (!hit) return null;
  const identity = {slug:slug(hit[1]), name:hit[1], focus:hit[2], recognized:true, dynamic:false};
  return {...identity, ...(FAMILY_METADATA[identity.slug] || {country:'未知', vendor:hit[1], source:null, confidence:'未知'})};
};
const familyFor = (model, dynamicNamespaces = null) => {
  const explicit = explicitFamilyFor(model);
  if (explicit) return explicit;
  const namespace = namespaceFor(model);
  if (dynamicNamespaces?.has(namespace)) {
    const dynamicSlug = `org-${slug(namespace) || 'unknown'}`;
    const metadata = FAMILY_METADATA[dynamicSlug] || {country:'未知', vendor:DYNAMIC_NAME_ALIASES[namespace] || namespace, source:null, confidence:'未知'};
    return {slug:dynamicSlug, namespace, name:DYNAMIC_NAME_ALIASES[namespace] || namespace, focus:'候选分组 · 基础模型或发布者 namespace', recognized:false, dynamic:true, ...metadata};
  }
  return {...(FAMILY_METADATA.other || {}), slug:'other', name:'Other', focus:'未归类模型', recognized:false, dynamic:false};
};
const variantFor = id => { const s=id.toLowerCase(); if(/(qlora|lora|adapter|peft)/.test(s))return 'Adapter'; if(/(gguf|gptq|awq|int[48]|quant)/.test(s))return '量化'; if(/(merge|merged|dare|slerp)/.test(s))return '合并'; if(/(finetune|fine-tune|instruct|chat|it$)/.test(s))return '指令/微调'; return '基础/其他'; };
const safeNum = value => Number.isFinite(Number(value)) ? Number(value) : 0;
const relationFor = (model, bases) => {
  if (bases.length) {
    const value = `${model.id} ${model.tags?.join(' ') || ''}`.toLowerCase();
    if (/(quant|gguf|gptq|awq|int4|int8)/.test(value)) return '量化';
    if (/(adapter|lora|qlora|peft)/.test(value)) return '适配器';
    if (/(merge|merged|slerp|dare)/.test(value)) return '合并';
    return '微调';
  }
  return '官方';
};
const authorFor = model => model.author || String(model.id || '').split('/')[0] || 'unknown';
const countryTagFor = model => (Array.isArray(model.tags) ? model.tags : [])
  .find(tag => typeof tag === 'string' && tag.startsWith('region:'))
  ?.slice('region:'.length) || null;
const ownerNamespaceFor = model => String(model.author || model.id || '').split('/')[0] || 'unknown';
const officialNamespacesFor = family => new Set(family?.official_namespaces || []);
const ownerTypeFor = () => 'unknown';
const ownershipFor = (family, model, bases) => {
  const namespace = ownerNamespaceFor(model);
  if (officialNamespacesFor(family).has(namespace)) return 'official';
  if (bases.length) return 'non_official';
  if (family.dynamic && namespace === family.namespace) return 'unknown';
  return 'non_official';
};

async function fetchModels() {
  const scan = await scanCatalog(store, 'models', HF_URL, {
    max: MAX_MODELS,
    pageSize: PAGE_SIZE,
    delay: REQUEST_DELAY_MS,
    token: process.env.HF_TOKEN,
  });
  return { ...scan, models: store.iterate(scan.run_id) };
}

async function snapshots(){ return store.reports().map(r => ({...r, at:r.generated_at, downloads:r.summary.downloads})); }
function trendFor(history, value) {
  if (history.length < 2) return { growth_pct: null, forecast_downloads_7d: null, confidence: '低' };
  const a = history.at(-2);
  const b = history.at(-1);
  const days = (new Date(b.at) - new Date(a.at)) / 86400000;
  if (days < 1 / 24) return { growth_pct: null, forecast_downloads_7d: null, confidence: '低' };
  const delta = safeNum(b.downloads) - safeNum(a.downloads);
  const pct = delta / Math.max(safeNum(a.downloads), 1) * 100;
  return {
    growth_pct: pct,
    forecast_downloads_7d: Math.max(0, value + delta * (7 / days)),
    confidence: history.length >= 4 ? '中' : '低',
  };
}

function insight(f) {
  if (f.growth_pct > 15) return '增长信号强：下载增速显著高于当前样本平均，值得优先关注其微调、量化和部署工具链。';
  if (f.count > 80 && f.downloads > 1e8) return '生态成熟：模型数量和使用规模都已形成平台效应，新项目应重点比较任务质量、许可证与推理成本。';
  if (f.count > 25 && f.growth_pct > 5) return '生态扩张：衍生模型正在增加，建议持续跟踪新 checkpoint 与应用领域分化。';
  if (f.growth_pct < 0) return '短期降温：最近快照下载量回落，可能是周期性波动或头部注意力迁移，需等待下一次刷新确认。';
  return '稳定观察：当前热度健康但未出现明显加速，适合用作基准家族持续跟踪。';
}
function serializeFamily(f) {
  return {
    slug: f.slug,
    grouping_kind: f.dynamic ? 'namespace_candidate' : 'curated_rule',
    namespace: f.namespace || null,
    name: f.name,
    focus: f.focus,
    count: f.count,
    downloads: f.downloads,
    likes: f.likes,
    forecast_downloads_7d: f.forecast_downloads_7d,
    forecast_confidence: f.forecast_confidence,
    growth_pct: f.growth_pct,
    signal: f.signal,
    insight: f.insight,
    history: f.history,
    country: f.country,
    vendor: f.vendor,
    identity_source: f.source,
    identity_confidence: f.confidence,
    official_namespaces: f.official_namespaces || [],
    num_authors: f.num_authors,
    official_count: f.official_count || 0,
    nonofficial_count: f.nonofficial_count || 0,
    organizations: [...f.organizations.entries()]
      .map(([name, value]) => ({name, ...value}))
      .sort((a,b)=>b.downloads-a.downloads),
    official_share: f.official_share,
    derived_count: f.derived_count,
    derived_share: f.derived_share,
    recent_count: f.recent_count,
    recent_models: f.recent_models,
    top10_share: f.top10_share,
    repository_regions: Object.fromEntries(f.repository_regions || []),
    models: f.models,
  };
}

function finalizeFamily(f) {
  f.models.sort((a, b) => b.downloads - a.downloads);
  f.models = f.models.slice(0, 100);
  const top10 = [...f.organizations.values()].map(v=>v.downloads).sort((a,b)=>b-a).slice(0,10).reduce((n,v)=>n+v,0);
  f.num_authors = f.authors.size;
  f.official_share = f.downloads ? f.official_downloads / f.downloads : 0;
  f.derived_share = f.count ? f.derived_count / f.count : 0;
  f.top10_share = f.downloads ? top10 / f.downloads : 0;
  return f;
}

function aggregate(models, history, at = new Date().toISOString(), dynamicNamespaces = null) {
  const groups = new Map();
  for (const raw of models) {
    if (!raw?.id) continue;
    const base = familyFor(raw, dynamicNamespaces);
    const f = groups.get(base.slug) || {
      ...base,
      models: [],
      authors: new Set(),
      author_downloads: new Map(),
      organizations: new Map(),
      repository_regions: new Map(),
      count: 0,
      official_count: 0,
      nonofficial_count: 0,
      downloads: 0,
      likes: 0,
      official_downloads: 0,
      derived_count: 0,
      recent_models: [],
      recent_count: 0,
    };
    const bases = basesFor(raw);
    const m = {
      id: raw.id,
      author: authorFor(raw),
      organization: ownerNamespaceFor(raw),
      ownership: ownershipFor(base, raw, bases),
      owner_type: ownerTypeFor(raw),
      repository_region: countryTagFor(raw),
      downloads: safeNum(raw.downloads),
      likes: safeNum(raw.likes),
      trending_score: safeNum(raw.trendingScore),
      created_at: raw.createdAt || null,
      last_modified: raw.lastModified || null,
      pipeline_tag: raw.pipeline_tag || 'unknown',
      relation: relationFor(raw, bases),
      variant: variantFor(raw.id),
      base_models: bases,
      derived_evidence: bases.length > 0,
      recent: Number.isFinite(Date.parse(raw.createdAt)) && Date.parse(raw.createdAt) <= Date.parse(at) && Date.parse(at) - Date.parse(raw.createdAt) <= 14 * 86400000,
    };
    f.models.push(m);
    if (f.models.length > 200) {
      f.models.sort((a,b)=>b.downloads-a.downloads);
      f.models.length = 100;
    }
    f.authors.add(m.author);
    f.organizations.set(m.organization, (f.organizations.get(m.organization) || {count:0, downloads:0, ownership:m.ownership}));
    const organization = f.organizations.get(m.organization);
    organization.count++;
    organization.downloads += m.downloads;
    if (m.repository_region) f.repository_regions.set(m.repository_region, (f.repository_regions.get(m.repository_region) || 0) + 1);
    if (m.ownership === 'official') f.official_count++;
    else if (m.ownership === 'non_official') f.nonofficial_count++;
    f.count++;
    f.downloads += m.downloads;
    f.likes += m.likes;
    if (m.ownership === 'official') f.official_downloads += m.downloads;
    if (m.derived_evidence) f.derived_count++;
    if (m.recent) {
      f.recent_count++;
      f.recent_models.push(m);
      f.recent_models.sort((a,b)=>b.trending_score-a.trending_score || a.id.localeCompare(b.id));
      f.recent_models.length = Math.min(f.recent_models.length, 20);
    }
    groups.set(base.slug, f);
  }
  return [...groups.values()].map((f) => {
    finalizeFamily(f);
    f.history = [
      ...history
        .filter((x) => x.families?.some((y) => y.slug === f.slug))
        .map((x) => ({
          at: x.at,
          downloads: x.families.find((y) => y.slug === f.slug)?.downloads || 0,
        })),
      { at, downloads: f.downloads },
    ];
    const trend = trendFor(f.history, f.downloads);
    f.growth_pct = trend.growth_pct;
    f.forecast_downloads_7d = trend.forecast_downloads_7d;
    f.forecast_confidence = trend.confidence;
    f.signal = f.growth_pct == null
      ? '待积累'
      : f.growth_pct > 15
        ? '加速中'
        : f.growth_pct > 5
          ? '温和增长'
          : f.growth_pct < 0
            ? '回落'
            : '稳定';
    f.insight = f.growth_pct == null
      ? '这是首次采集，下一次快照后将开始计算增速；当前先用规模、下载量和衍生模型结构判断生态位置。'
      : insight(f);
    return f;
  }).sort((a, b) => {
    if (a.slug === 'other' && b.slug !== 'other') return 1;
    if (a.slug !== 'other' && b.slug === 'other') return -1;
    return b.downloads - a.downloads;
  });
}

function familiesFromSnapshot(snapshot) {
  return (snapshot.families || []).map((f) => ({
    ...f,
    models: f.models || [],
    history: [],
    growth_pct: null,
    signal: '待刷新',
    insight: '当前使用本地缓存；刷新成功后将恢复趋势计算。',
  }));
}

async function scanOrReuseCompleted() {
  const latest = store.jobs().find(job => job.kind === 'models');
  if (latest?.status === 'complete' && latest.cursor === null) {
    return {
      run_id: latest.id,
      kind: 'models',
      pages: latest.pages,
      complete: true,
      stop_reason: 'end_of_catalog',
      duration_ms: latest.updated - latest.started,
      total_elapsed_ms: latest.updated - latest.started,
      count: latest.count,
      models: store.iterate(latest.id),
      reused: true,
    };
  }
  return fetchModels();
}

async function refresh() {
  if (refreshPromise) return refreshPromise;
  refreshPromise = (async () => {
    const executionStarted = Date.now();
    const scan = await scanOrReuseCompleted();
    if (scan.unchanged && current) return { ...current, unchanged: true };
    const aggregationStarted = Date.now();
    const old = await snapshots();
    const at = new Date().toISOString();
    const families = await summarizeRun(scan.run_id, at, true);
    const comparable = scan.complete ? old.filter(r => r.summary.complete) : [];
    for (const f of families) {
      f.history = [...comparable.flatMap(r => {
        const prior = r.families.find(p => p.slug === f.slug);
        return prior ? [{at:r.generated_at, downloads:prior.downloads}] : [];
      }), {at, downloads:f.downloads}];
      const trend = trendFor(f.history, f.downloads);
      f.growth_pct = trend.growth_pct;
      f.forecast_downloads_7d = trend.forecast_downloads_7d;
      f.forecast_confidence = trend.confidence;
      f.signal = trend.growth_pct == null ? '待积累' : trend.growth_pct > 0 ? '增长' : trend.growth_pct < 0 ? '回落' : '稳定';
      f.insight = trend.growth_pct == null ? '需要两次完整目录扫描才能比较趋势。' : insight(f);
    }
    if (families.reduce((n,f)=>n+f.count,0) !== scan.count) throw new Error('Report count does not match committed collection');
    const {models: _iterator, ...scanInfo} = scan;
    const snapshot = {
      generated_at: at,
      summary: {
        model_count: families.reduce((n,f)=>n+f.count,0),
        downloads: families.reduce((n,f)=>n+f.downloads,0),
        likes: families.reduce((n,f)=>n+f.likes,0),
        family_count: families.length,
        pages: scan.pages,
        complete: scan.complete,
        stop_reason: scan.stop_reason,
        duration_ms: scan.duration_ms,
        growth: old.at(-1)?.summary?.downloads
          ? ((families.reduce((n,f)=>n+f.downloads,0) - old.at(-1).summary.downloads) / old.at(-1).summary.downloads) * 100
          : null,
      },
      jobs: { models: scanInfo },
      families: families.map(serializeFamily),
    };
    store.saveReport(snapshot, {
      collection_ms: scan.duration_ms,
      aggregation_ms: Date.now() - aggregationStarted,
      execution_started: executionStarted,
      run_started: store.jobs().find(job => job.id === scan.run_id).started,
    });
    current = { ...snapshot, stale: false, error: null, families };
    return current;
  })().catch(async (error) => {
    const old = await snapshots();
    if (!old.length) throw error;
    const last = old.at(-1);
    current = { ...last, stale: true, error: `刷新失败：${error.message}` };
    return current;
  }).finally(() => {
    refreshPromise = null;
  });
  return refreshPromise;
}

function publicInsights() {
  return {
    ...current,
    refresh: {
      interval: REFRESH_INTERVAL_LABEL,
      interval_ms: REFRESH_MS,
      next_at: new Date(nextRefreshAt).toISOString(),
      running: Boolean(refreshPromise),
      automatic_refreshes: automaticRefreshes,
      full_scan: MAX_MODELS === 0,
      model_limit: MAX_MODELS || null,
    },
  };
}

const membershipGeneration = run => `namespace-v2:${run}:${DYNAMIC_MIN_MODELS}:${DYNAMIC_MIN_DOWNLOADS}`;
const liveSummaries = new Map();
const namespaceCache = new Map();
async function discoverDynamicNamespaces(run) {
  const stats = new Map();
  let after = '';
  for (;;) {
    const batch = store.batch(run, after, 1000);
    if (!batch.length) break;
    for (const raw of batch) {
      if (explicitFamilyFor(raw)) continue;
      const namespace = namespaceFor(raw);
      const stat = stats.get(namespace) || {count:0, downloads:0};
      stat.count++;
      stat.downloads += safeNum(raw.downloads);
      stats.set(namespace, stat);
    }
    after = batch.at(-1).id;
    await new Promise(resolve => setImmediate(resolve));
  }
  const namespaces = new Set([...stats.entries()]
    .filter(([, stat]) => stat.count >= DYNAMIC_MIN_MODELS || stat.downloads >= DYNAMIC_MIN_DOWNLOADS)
    .map(([namespace]) => namespace));
  namespaceCache.set(run, namespaces);
  return namespaces;
}
async function summarizeRun(run, at, force = false, dynamicNamespaces = null) {
  const cached = liveSummaries.get(run);
  if (!force && cached && (cached.pending || Date.now() - cached.at < 60000)) return cached.value;
  const value = (async () => {
    const namespaces = dynamicNamespaces || await discoverDynamicNamespaces(run);
    const generation = membershipGeneration(run);
    const indexReady = store.hasIndex(generation);
    const merged = new Map();
    let after = '';
    for (;;) {
      const batch = store.batch(run, after, 1000);
      if (!batch.length) break;
      if (!indexReady) store.indexMembers(generation, batch.map(model => ({family:familyFor(model, namespaces).slug,id:model.id})));
      for (const f of aggregate(batch, [], at, namespaces)) {
        const previous = merged.get(f.slug);
        if (!previous) merged.set(f.slug, f);
        else {
          previous.count += f.count;
          previous.downloads += f.downloads;
          previous.likes += f.likes;
          previous.official_downloads += f.official_downloads;
          previous.official_count += f.official_count;
          previous.nonofficial_count += f.nonofficial_count;
          for (const [name, value] of f.organizations) {
            const target = previous.organizations.get(name) || {count:0, downloads:0, ownership:value.ownership};
            target.count += value.count;
            target.downloads += value.downloads;
            previous.organizations.set(name, target);
          }
          previous.derived_count += f.derived_count;
          previous.recent_count += f.recent_count;
          previous.recent_models = previous.recent_models.concat(f.recent_models).sort((a,b)=>b.trending_score-a.trending_score || a.id.localeCompare(b.id)).slice(0,20);
          for (const author of f.authors) previous.authors.add(author);
          for (const [region, count] of f.repository_regions) previous.repository_regions.set(region, (previous.repository_regions.get(region) || 0) + count);
          previous.models = previous.models.concat(f.models).sort((a,b)=>b.downloads-a.downloads).slice(0,100);
        }
      }
      after = batch.at(-1).id;
      await new Promise(resolve => setImmediate(resolve));
    }
    if (store.jobs().find(job => job.id === run)?.status === 'complete') store.finishIndex(generation);
    return [...merged.values()].map(f => finalizeFamily(f)).sort((a,b)=>(a.slug==='other')-(b.slug==='other') || b.downloads-a.downloads);
  })();
  const entry = {at:Date.now(), value, pending:true};
  liveSummaries.set(run, entry);
  try {
    const result = await value;
    entry.pending = false;
    entry.at = Date.now();
    return result;
  } catch (error) {
    if (liveSummaries.get(run) === entry) liveSummaries.delete(run);
    throw error;
  }
}

async function getInsights(){
  const completed = store.jobs().find(job => job.kind === 'models' && job.status === 'complete' && job.cursor === null);
  if (completed && !store.hasIndex(membershipGeneration(completed.id))) {
    await refresh();
    return publicInsights();
  }
  const job = store.jobs().find(job => job.kind === 'models' && job.status === 'running');
  if (job) {
    const families = await summarizeRun(job.id, new Date(job.updated).toISOString());
    const live = {
      generated_at: new Date(job.updated).toISOString(),
      summary: {
        model_count: families.reduce((n,f)=>n+f.count,0),
        family_count: families.length,
        downloads: families.reduce((n,f)=>n+f.downloads,0),
        likes: families.reduce((n,f)=>n+f.likes,0),
        pages: job.pages,
        complete: false,
        duration_ms: Date.now() - job.started,
        growth: null,
      },
      families,
      collecting: true,
      error: job.error || null,
    };
    return {...live, refresh: publicInsights().refresh};
  }
  if (!current) await refresh();
  return publicInsights();
}
async function familyOrganizations(slug, offset = 0) {
  const job = store.jobs().find(j => j.id === current?.jobs?.models?.run_id);
  if (!job || !store.hasIndex(membershipGeneration(job.id))) throw new Error('分组索引尚未就绪');
  const official = (FAMILY_METADATA[slug]?.official_namespaces || []).map(x=>x.toLowerCase());
  const rows = store.memberOrganizations(job.id,membershipGeneration(job.id),slug,offset,21);
  const organizations = rows.slice(0,20).map(row=>({...row,ownership:official.length ? (official.includes(row.name.toLowerCase())?'official':'non_official') : 'unknown'}));
  return {organizations, offset, limit:20, next:rows.length>20?offset+20:null, previous:offset>0?Math.max(0,offset-20):null, official_known:official.length>0, observed_at:current.generated_at};
}
async function familyPage(slug, offset, organization = null) {
  const job = store.jobs().find(j => j.id === current?.jobs?.models?.run_id);
  if (!job || !store.hasIndex(membershipGeneration(job.id))) throw new Error('分组索引尚未就绪，请稍后重试');
  const rows = store.memberPage(job.id, membershipGeneration(job.id), slug, offset, 21, organization);
  return {items:rows.slice(0,20).map(raw=>({id:raw.id,author:authorFor(raw),downloads:safeNum(raw.downloads),likes:raw.likes==null?null:safeNum(raw.likes),created_at:raw.createdAt||null,relation:basesFor(raw).length?relationFor(raw,basesFor(raw)):'未声明衍生关系',base_models:basesFor(raw)})),offset,limit:20,next:rows.length>20?offset+20:null};
}
function staticFile(res,path){ if(path==='/detail.js'){readFile(join(ROOT,'detail.js')).then(body=>{res.writeHead(200,{'content-type':'text/javascript; charset=utf-8','cache-control':'no-cache'});res.end(body)}).catch(()=>json(res,500,{error:'read failed'}));return;} if(!path.startsWith('/api/')){readFile(join(ROOT,'index.html')).then(body=>{res.writeHead(200,{'content-type':'text/html; charset=utf-8'});res.end(body)}).catch(()=>json(res,500,{error:'read failed'}));return;} return json(res,404,{error:'not found'}); }
const server=createServer(async(req,res)=>{ try { const u=new URL(req.url||'/',`http://${req.headers.host||HOST}`); if(req.method==='GET'&&u.pathname==='/api/insights')return json(res,200,await getInsights()); if(req.method==='GET'&&u.pathname==='/api/family-models'){const offset=Number(u.searchParams.get('offset')||0);const organization=u.searchParams.get('organization')||null;if(!Number.isSafeInteger(offset)||offset<0)return json(res,400,{error:'Invalid offset'});return json(res,200,await familyPage(u.searchParams.get('family'),offset,organization));} if(req.method==='GET'&&u.pathname==='/api/family-organizations'){const offset=Number(u.searchParams.get('offset')||0);if(!Number.isSafeInteger(offset)||offset<0)return json(res,400,{error:'Invalid offset'});return json(res,200,await familyOrganizations(u.searchParams.get('family'),offset));} if(req.method==='GET'&&u.pathname==='/api/jobs')return json(res,200,{jobs:store.jobs(),refresh_running:Boolean(refreshPromise)}); if(req.method==='POST'&&u.pathname==='/api/refresh')return json(res,200,await refresh()); if(req.method==='GET'&&u.pathname.startsWith('/api/families/')){const data=await getInsights(),f=data.families.find(x=>x.slug===u.pathname.split('/').pop());return f?json(res,200,f):json(res,404,{error:'family not found'});} if(req.method==='GET')return staticFile(res,u.pathname); json(res,405,{error:'method not allowed'}); }catch(e){json(res,502,{error:e.message||'service unavailable'});} });
const shutdown = () => {
  store.close();
  process.exit(0);
};
process.once('SIGINT', shutdown);
process.once('SIGTERM', shutdown);
export { fetchModels, aggregate, trendFor, familiesFromSnapshot };
if (process.argv[1] && fileURLToPath(import.meta.url) === fileURLToPath(new URL(process.argv[1], `file://${process.cwd()}/`))) {
  if (process.argv.includes('--refresh-once')) {
    refresh().then(data => { if (data.stale) process.exitCode = 1; }).catch(error => { console.error(error.message); process.exitCode = 1; });
  } else {
    server.listen(PORT,HOST,()=>{console.log(`HF Model Oracle listening on http://${HOST}:${PORT}`);if(!current)refresh().catch(e=>console.error(`Initial refresh failed: ${e.message}`));});
    nextRefreshAt = Date.now() + REFRESH_MS;
    setInterval(() => {
      nextRefreshAt = Date.now() + REFRESH_MS;
      if (refreshPromise) return;
      refresh()
        .then(result => { if (!result.stale && !result.unchanged) automaticRefreshes++; })
        .catch(e => console.error(`Scheduled refresh failed: ${e.message}`));
    }, REFRESH_MS).unref();
  }
}
