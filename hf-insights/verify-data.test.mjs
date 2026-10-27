import test from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, rmSync} from 'node:fs';
import {join} from 'node:path';
import {Store} from './store.mjs';

 test('data acceptance rejects partial and inconsistent reports', () => {
  const dir = mkdtempSync(new URL('./data/acceptance-', import.meta.url));
  const path = join(dir, 'test.sqlite');
  const store = new Store(path);
  const verify = () => spawnSync(process.execPath, [new URL('./verify-data.mjs', import.meta.url).pathname], {
    env: {...process.env, HF_INSIGHTS_DB:path, HF_INSIGHTS_VERIFY_WAIT_SECONDS:'0'}, encoding:'utf8', timeout:30000,
  });
  try {
    const run = store.begin('models', 'https://hf.test/api/models');
    store.page(run.id, [{id:'Qwen/test', downloads:10, likes:2}], 'https://hf.test/next', 'running');
    assert.equal(verify().status, 2);
    store.page(run.id, [], null, 'complete');
    assert.equal(verify().status, 2, 'a completed scan without a report is not accepted');
    store.saveReport({summary:{complete:true,model_count:99},jobs:{models:{run_id:run.id}},families:[{count:1}]});
    assert.equal(verify().status, 2, 'wrong report count must fail');
    store.saveReport({summary:{complete:true,model_count:1},jobs:{models:{run_id:run.id}},families:[{count:1}]});
    const result=verify();
    assert.equal(result.status,0,result.stderr);
    assert.equal(JSON.parse(result.stdout).passed,true);
  } finally {
    store.close();
    rmSync(dir,{recursive:true,force:true});
  }
});
