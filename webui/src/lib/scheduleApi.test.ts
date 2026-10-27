import { test } from 'node:test';
import assert from 'node:assert/strict';
Object.defineProperty(globalThis,'location',{value:{pathname:'/',hash:'',search:'?token=schedule-test-only'},configurable:true});
const { changeSchedule,runSchedule,listSchedules,scheduleHistory,watchSchedules }=await import('./scheduleApi.ts');
const task={id:'test',title:'Review',prompt:'Summarize',cwd:'/project',schedule:{kind:'daily' as const,time:'09:00'},permission_mode:'plan',notify:'important',enabled:true,created_at:0};

test('mutation and run requests retain operation ids and observed revisions',async()=>{
  const calls:Array<{path:unknown;body:Record<string,unknown>;headers:Headers}>=[];
  const original=globalThis.fetch;
  globalThis.fetch=async(path,init)=>{calls.push({path,body:JSON.parse(String(init?.body)),headers:new Headers(init?.headers)});return Response.json({entry:null,deleted_id:null,state:'accepted'});};
  try {
    await changeSchedule('stable-operation',{action:'update',task,expected:'observed-revision'});
    await runSchedule('test','run-operation','observed-revision');
    assert.equal(calls[0].path,'/schedules');assert.equal(calls[0].body.request_id,'stable-operation');
    assert.equal(calls[0].body.expected,'observed-revision');assert.equal(calls[0].headers.get('Authorization'),'Bearer schedule-test-only');
    assert.equal(calls[1].path,'/schedules/test/run');assert.equal(calls[1].body.expected,'observed-revision');assert.equal(calls[1].body.request_id,'run-operation');
  } finally {globalThis.fetch=original;}
});
test('failed and malformed catalogs are errors rather than an empty task list',async()=>{
  const original=globalThis.fetch;let calls=0;
  try {
    globalThis.fetch=async()=>{calls++;return Response.json({error:'service unavailable'},{status:503});};
    await assert.rejects(listSchedules(),/service unavailable/);assert.equal(calls,1,'no automatic retry');
    globalThis.fetch=async()=>Response.json({});
    await assert.rejects(listSchedules(),/Invalid automation catalog/);
  } finally {globalThis.fetch=original;}
});
test('history cursor is a read-only query and subscription parses split CRLF chunks',async()=>{
  const original=globalThis.fetch;const paths:string[]=[];let changes=0;
  globalThis.fetch=async(path,init)=>{
    paths.push(String(path));assert.equal(init?.method===undefined||init.method==='GET',true);
    if(String(path).endsWith('/events')) {
      const encoder=new TextEncoder();
      return new Response(new ReadableStream({start(controller){for(const text of ['da','ta: {"revision":"one"}\r','\n\r\n: ping\r\n\r\n'])controller.enqueue(encoder.encode(text));controller.close();}}),{headers:{'Content-Type':'text/event-stream'}});
    }
    return Response.json({events:[],next_cursor:null});
  };
  try {
    await scheduleHistory('task',undefined,20);
    await watchSchedules(()=>{changes++;},new AbortController().signal);
    assert.equal(paths[0],'/schedules/task/history?cursor=20');assert.equal(changes,1);
  } finally {globalThis.fetch=original;}
});
