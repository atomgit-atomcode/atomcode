import { test } from 'node:test';
import assert from 'node:assert/strict';
import { foldScheduleRuns } from './scheduleHistory.ts';
test('run history uses saved instructions and append order across clock rollback',()=>{
  const runs=foldScheduleRuns([
    {kind:'started',run_id:'old',at:100,prompt:'Original instruction'},
    {kind:'session',run_id:'old',at:101,session_id:'native-session'},
    {kind:'finished',run_id:'old',at:102,status:'ok'},
    {kind:'started',run_id:'new',at:50,prompt:'Edited instruction'},
    {kind:'finished',run_id:'new',at:51,status:'error',error:'provider unavailable'},
  ]);
  assert.equal(runs.length,2);
  assert.equal(runs[0].id,'new');assert.equal(runs[0].status,'error');
  assert.equal(runs[1].prompt,'Original instruction');assert.equal(runs[1].sessionId,'native-session');
});
test('partial history does not invent a missing prompt or start time',()=>{
  const [run]=foldScheduleRuns([{kind:'finished',run_id:'partial',at:1,status:'interrupted'}]);
  assert.equal(run.prompt,undefined);assert.equal(run.startedAt,undefined);assert.equal(run.status,'interrupted');
});
