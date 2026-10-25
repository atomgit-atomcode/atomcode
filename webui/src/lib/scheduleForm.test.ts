import { test } from 'node:test';
import assert from 'node:assert/strict';
import { selectRule, clockInputValue, orderedSchedules, rebaseSchedule, sameTask, scheduleProblem } from './scheduleForm.ts';
import type { ScheduleEntry } from './scheduleApi.ts';
function entry(): ScheduleEntry { return { task:{id:'task',title:'Review',prompt:'Read changes',cwd:'/project',schedule:{kind:'daily',time:'09:00'},permission_mode:'plan',notify:'important',enabled:true,created_at:1},revision:'old',registration:'registered',registration_error:null,next_run:{at:10,time_zone:'host local',estimated:true},last_run:null,running:false }; }
test('refresh preserves edited instruction and accepts unrelated remote changes with their revision',()=>{
  const base=entry(), draft=structuredClone(base.task), incoming=entry();draft.prompt='Local instruction';incoming.task.title='Remote title';incoming.revision='new';
  const result=rebaseSchedule(base,draft,incoming);assert.deepEqual(result.conflicts,[]);assert.equal(result.draft.title,'Remote title');assert.equal(result.draft.prompt,'Local instruction');
  assert.equal(sameTask(result.draft,incoming.task),false);
});
test('overlapping changes conflict, identical concurrent edits do not, and rule key order is immaterial',()=>{
  const base=entry(), draft=structuredClone(base.task), incoming=entry();draft.prompt='Local';incoming.task.prompt='Remote';
  assert.deepEqual(rebaseSchedule(base,draft,incoming).conflicts,['prompt']);incoming.task.prompt='Local';assert.deepEqual(rebaseSchedule(base,draft,incoming).conflicts,[]);
  incoming.task.schedule={time:'09:00',kind:'daily'};assert.equal(sameTask(base.task,{...incoming.task,prompt:base.task.prompt}),true);
});
test('local validation rejects zero intervals and invalid clock/weekday before save',()=>{
 const t=entry().task;t.schedule={kind:'interval',every_minutes:0};assert.equal(scheduleProblem(t),'interval');t.schedule={kind:'daily',time:'24:00'};assert.equal(scheduleProblem(t),'time');t.schedule={kind:'weekly',weekday:8,time:'09:00'};assert.equal(scheduleProblem(t),'weekday');t.schedule={kind:'hourly'};assert.equal(scheduleProblem(t),null);t.prompt='你'.repeat(22000);assert.equal(scheduleProblem(t),'prompt');
});
test('catalog trims search and sorts active tasks by next occurrence without changing server data',()=>{
 const later=entry(), earlier=entry(), paused=entry();later.task.id='later';later.next_run!.at=20;earlier.task.id='earlier';paused.task.id='paused';paused.task.enabled=false;
 const rows=[paused,later,earlier];assert.deepEqual(orderedSchedules(rows,'  review  ','all').map(e=>e.task.id),['earlier','later','paused']);assert.equal(rows[0],paused);assert.equal(orderedSchedules(rows,'','disabled').length,1);
});

test('legacy clock spellings stay editable without changing their stored rule',()=>{const task=entry().task;task.schedule={kind:'daily',time:'9:0'};assert.equal(clockInputValue(task.schedule.time),'09:00');assert.equal(scheduleProblem(task),null);assert.equal(task.schedule.time,'9:0');});

test('changing daily/weekly keeps the chosen clock and selecting an existing interval is a no-op',()=>{assert.deepEqual(selectRule({kind:'daily',time:'17:45'},'weekly'),{kind:'weekly',weekday:1,time:'17:45'});assert.deepEqual(selectRule({kind:'weekly',weekday:5,time:'07:15'},'daily'),{kind:'daily',time:'07:15'});const interval={kind:'interval' as const,every_minutes:120};assert.equal(selectRule(interval,'interval'),interval);});
