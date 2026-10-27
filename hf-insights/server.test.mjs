import test from 'node:test';
import assert from 'node:assert/strict';
process.env.HF_INSIGHTS_DB = ':memory:';
const { aggregate, trendFor, familiesFromSnapshot, fetchModels } = await import('./server.mjs');
import { requestPage, scanCatalog } from './collector.mjs';
import { Store } from './store.mjs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { mkdtemp, rm } from 'node:fs/promises';

test('organizations paginate twenty at a time in download order', () => {
  const db = new Store(':memory:');
  try {
    const run = db.begin('models','https://hf.test/models');
    const rows = Array.from({length:45},(_,i)=>({id:`owner${i}/model`,downloads:100-i,likes:i}));
    db.page(run.id,rows,null,'complete');
    db.indexMembers('test',rows.map(r=>({family:'qwen',id:r.id})));
    const first=db.memberOrganizations(run.id,'test','qwen',0,21);
    const second=db.memberOrganizations(run.id,'test','qwen',20,21);
    const last=db.memberOrganizations(run.id,'test','qwen',40,21);
    assert.equal(first.length,21);
    assert.equal(second.length,21);
    assert.equal(last.length,5);
    assert.equal(first[20].name,second[0].name);
    assert.equal(first.slice(0,20).some(a=>second.slice(0,20).some(b=>a.name===b.name)),false);
    assert.equal(first[0].downloads,100);
    assert.equal(db.memberOrganizations(run.id,'test','qwen',60,21).length,0);
  } finally { db.close(); }
});

test('cursor pagination deduplicates and preserves lineage', async () => {
  const original = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async () => {
    calls++;
    return new Response(JSON.stringify(calls === 1
      ? [{id:'Qwen/base', downloads:100, likes:10}]
      : [{id:'Qwen/base', downloads:100, likes:10}, {id:'alice/tuned', downloads:20, cardData:{base_model:'Qwen/base'}}]), {
      headers: calls === 1 ? {link:'<https://huggingface.co/api/models?cursor=next>; rel="next"'} : {},
    });
  };
  try {
    const scan = await fetchModels();
    const models = [...scan.models];
    assert.equal(scan.pages, 2);
    assert.equal(models.length, 2);
    assert.equal(scan.complete, true);
    const family = aggregate(models, [])[0];
    assert.equal(family.slug, 'qwen');
    assert.equal(family.count, 2);
    assert.equal(family.models.filter(model => model.derived_evidence).length, 1);
    assert.equal(family.growth_pct, null);
    const other = aggregate([...models, {id:'unknown/repo', downloads:999999}], [])[1];
    assert.equal(other.slug, 'other');
  } finally { globalThis.fetch = original; }
});

test('discovered namespaces group publisher and declared descendants', () => {
  const rows = [
    {id:'stepfun-ai/Step-3.5',downloads:10},
    {id:'someone/quant',downloads:20,tags:['base_model:quantized:stepfun-ai/Step-3.5']},
    {id:'someone/unrelated',downloads:5},
  ];
  const families = aggregate(rows, [], '2026-10-20', new Set(['stepfun-ai']));
  const candidate = families.find(f=>f.slug==='org-stepfun-ai');
  assert.equal(candidate.count,2);
  assert.equal(candidate.downloads,30);
  assert.equal(candidate.dynamic,true);
  assert.equal(candidate.namespace,'stepfun-ai');
  assert.equal(families.find(f=>f.slug==='other').count,1);
});

test('streamed family totals include every model while details stay bounded', () => {
  function* models() {
    for (let i = 0; i < 10000; i++) yield {id:`Qwen/model-${i}`, downloads:i, likes:1};
  }
  const family = aggregate(models(), [])[0];
  assert.equal(family.count, 10000);
  assert.equal(family.likes, 10000);
  assert.equal(family.downloads, 9999 * 10000 / 2);
  assert.equal(family.models.length, 100);
  assert.equal(family.models[0].downloads, 9999);
  assert.equal(family.models[99].downloads, 9900);
});

test('recent heat ranking is independent of download ranking', () => {
  const rows = Array.from({length:250}, (_,i)=>({id:`deepseek-ai/old-${i}`, downloads:100000+i, createdAt:'2025-01-01', trendingScore:1}));
  rows.push({id:'deepseek-ai/DeepSeek-V4.1-Flash-1017',downloads:17924,createdAt:'2026-10-17T02:40:38Z',trendingScore:1562});
  rows.push({id:'deepseek-ai/future',downloads:1,createdAt:'2026-10-21',trendingScore:9999});
  const family=aggregate(rows, [], '2026-10-20T12:00:00Z')[0];
  assert.equal(family.models.some(m=>m.id.endsWith('1017')),false);
  assert.equal(family.recent_models[0].id,'deepseek-ai/DeepSeek-V4.1-Flash-1017');
  assert.equal(family.recent_count,1);
});

test('compact requests preserve cursor, metrics and lineage', async () => {
  const previous = process.env.HF_INSIGHTS_COMPACT;
  process.env.HF_INSIGHTS_COMPACT = '1';
  const db = new Store(':memory:');
  const cursors = [];
  try {
    const result = await scanCatalog(db, 'models', 'https://hf.test/models', {
      delay:0,
      request:async url => {
        assert.equal(url.searchParams.has('full'), false);
        cursors.push(url.searchParams.get('cursor'));
        return new Response(JSON.stringify([{id:`Qwen/${cursors.length}`, downloads:10, likes:2, tags:['base_model:Qwen/base']}]), {
          headers:cursors.length===1 ? {link:'<https://hf.test/models?cursor=second&full=true>; rel="next"'} : {},
        });
      },
    });
    assert.deepEqual(cursors,[null,'second']);
    assert.equal(result.complete,true);
    const family=aggregate(db.iterate(result.run_id),[])[0];
    assert.equal(family.downloads,20);
    assert.equal(family.likes,4);
    assert.equal(family.models.every(m=>m.derived_evidence),true);
  } finally {
    db.close();
    if(previous===undefined)delete process.env.HF_INSIGHTS_COMPACT;
    else process.env.HF_INSIGHTS_COMPACT=previous;
  }
});

test('daily trend extrapolates seven days and rejects short intervals', () => {
  const points = [{at:'2026-10-01T00:00:00Z',downloads:100},{at:'2026-10-02T00:00:00Z',downloads:110}];
  assert.equal(trendFor(points,110).forecast_downloads_7d,180);
  assert.equal(trendFor(points,110).growth_pct,10);
  points[1].at='2026-10-01T00:01:00Z';
  assert.equal(trendFor(points,110).forecast_downloads_7d,null);
});

test('cached families preserve real identifiers and likes', () => {
  const model = {id:'Qwen/real',likes:17};
  const restored = familiesFromSnapshot({families:[{slug:'qwen',likes:17,models:[model]}]});
  assert.deepEqual(restored[0].models,[model]);
  assert.equal(restored[0].likes,17);
});


test('requestPage retries rate limits and honors Retry-After', async () => {
  let calls = 0;
  const waits = [];
  const response = await requestPage('https://hf.test/models', {
    fetcher: async () => {
      calls++;
      if (calls === 1) return new Response('busy', { status: 429, headers: { 'retry-after': '1' } });
      return new Response('[]', { status: 200 });
    },
    wait: async ms => waits.push(ms),
  });
  assert.equal(response.status, 200);
  assert.equal(calls, 2);
  assert.deepEqual(waits, [1000]);
});

test('requestPage retries a broken body with a smaller page and the same cursor', async () => {
  const seen = [];
  const result = await requestPage('https://hf.test/models?limit=1000&cursor=abc', {
    wait: async () => {},
    fetcher: async url => {
      const parsed = new URL(url);
      seen.push([parsed.searchParams.get('limit'), parsed.searchParams.get('cursor')]);
      if (seen.length === 1) return {ok:true, text:async()=>{throw new Error('body timeout');}, headers:new Headers()};
      return new Response('[{"id":"Qwen/model"}]', {headers:{link:'<https://hf.test/models?cursor=next>; rel="next"'}});
    },
  });
  assert.deepEqual(seen, [['1000','abc'],['100','abc']]);
  assert.equal((await result.json())[0].id, 'Qwen/model');
  assert.ok(result.headers.get('link').includes('cursor=next'));
});

test('scanCatalog persists only valid rows and resumes from the cursor', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'hf-oracle-test-'));
  const store = new Store(join(dir, 'test.sqlite'));
  let calls = 0;
  const request = async url => {
    calls++;
    const page = calls === 1
      ? [{id:'valid/one', downloads:1}, {id:'bad/no-id', downloads:-1}]
      : [{id:'valid/two', downloads:2}];
    return new Response(JSON.stringify(page), {
      status: 200,
      headers: calls === 1 ? {link:'<https://hf.test/models?cursor=two>; rel="next"'} : {},
    });
  };
  try {
    const first = await scanCatalog(store, 'models', 'https://hf.test/models', {max:1, pageSize:2, delay:0, request});
    assert.equal(first.count, 1);
    assert.equal(first.stop_reason, 'configured_budget');
    assert.equal(store.rows(first.run_id).length, 1);
    const second = await scanCatalog(store, 'models', 'https://hf.test/models', {max:2, pageSize:2, delay:0, request});
    assert.equal(second.count, 2);
    assert.equal(second.pages, 2);
    assert.deepEqual(store.rows(second.run_id).map(row => row.id), ['valid/one', 'valid/two']);
    assert.equal(store.jobs()[0].rejected, 1);
  } finally {
    store.close();
    await rm(dir, {recursive:true, force:true});
  }
});
