import { test } from 'node:test';
import assert from 'node:assert/strict';
Object.defineProperty(globalThis, 'location', { value: new URL('http://localhost/'), configurable: true });

test('hooks inspection is read-only and a test sends only the exact selection ID', async () => {
  const previous = globalThis.fetch;
  const calls: Array<{url:string;init?:RequestInit}> = [];
  try {
    const { getHooks, testHook } = await import('./api.ts');
    globalThis.fetch = async (url,init) => {
      calls.push({url:String(url),init});
      return Response.json({cwd:'/project',files:[],plugins:[],items:[]});
    };
    await getHooks();
    assert.equal(calls[0].url,'/hooks');
    assert.equal(calls[0].init?.method,undefined);
    await testHook('content-bound-id');
    assert.equal(calls[1].url,'/hooks/test');
    assert.equal(calls[1].init?.method,'POST');
    assert.deepEqual(JSON.parse(String(calls[1].init?.body)),{id:'content-bound-id'});
    globalThis.fetch = async () => Response.json({error:'configuration changed'},{status:400});
    await assert.rejects(testHook('stale-id'),/configuration changed/);
    globalThis.fetch = async () => Response.json([]);
    await assert.rejects(getHooks(),/Invalid hooks catalogue/);
  } finally {globalThis.fetch=previous;}
});
