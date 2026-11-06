import { test } from 'node:test';
import assert from 'node:assert/strict';
Object.defineProperty(globalThis, 'location', { value: new URL('http://localhost/'), configurable: true });

test('skill requests preserve HTTP errors and reject a successful non-list payload', async () => {
  const original = globalThis.fetch;
  try {
    const { getSkills } = await import('./api.ts');
    globalThis.fetch = async () => Response.json({ error: 'skill service unavailable' }, { status: 503 });
    await assert.rejects(getSkills(), /skill service unavailable/);
    globalThis.fetch = async () => Response.json({ error: 'not a list' });
    await assert.rejects(getSkills(), /Invalid skill list response/);
    globalThis.fetch = async () => Response.json([{ name: 'plugin:skill', description: 'usable skill' }]);
    assert.deepEqual(await getSkills(), [{ name: 'plugin:skill', description: 'usable skill' }]);
  } finally { globalThis.fetch = original; }
});
