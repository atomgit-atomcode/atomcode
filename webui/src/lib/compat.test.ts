import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { missingCssSince, signedIn } from './compat.ts';

test('a browser missing what the page uses is told the version that has it', () => {
  // Chromium 99 (Huawei's browser): none of the three.
  assert.equal(missingCssSince(() => false), 111);
  // Has color-mix and dvh, lacks :has() — Chromium 104-ish.
  assert.equal(missingCssSince((c) => !c.startsWith('selector(')), 105);
  // A current browser: nothing to say.
  assert.equal(missingCssSince(() => true), null);
  // A browser that throws on an unknown condition counts it as missing.
  assert.equal(
    missingCssSince((c) => {
      if (c.startsWith('selector(')) throw new SyntaxError('no selector()');
      return true;
    }),
    105,
  );
});

test('only a definite 401 means the page was opened without the link', async () => {
  const replying = (status: number) => (async () => new Response('{}', { status })) as typeof fetch;
  assert.equal(await signedIn(replying(401)), false);
  assert.equal(await signedIn(replying(200)), true);
  assert.equal(await signedIn(replying(500)), true);
  const down = (async () => {
    throw new TypeError('network down');
  }) as typeof fetch;
  assert.equal(await signedIn(down), true, 'a network error lets the app report for itself');
});

test('the probe carries the token from the page address when there is one', async () => {
  let sent: HeadersInit | undefined;
  const recording = (async (_url: RequestInfo | URL, init?: RequestInit) => {
    sent = init?.headers;
    return new Response('{}', { status: 200 });
  }) as typeof fetch;
  await signedIn(recording, 'tok-1');
  assert.equal((sent as Record<string, string>).Authorization, 'Bearer tok-1');
});

/**
 * Every `dvh` and every `color-mix()` in an ordinary declaration has a fallback
 * declaration right before it, which an engine without them (Chromium < 108 /
 * < 111) uses instead of dropping the property. The root layout's `100dvh`
 * without one is what made the whole page unusable on Huawei's browser.
 */
test('every dvh and color-mix declaration carries a fallback an old engine can use', () => {
  const root = process.cwd();
  for (const file of ['src/styles/app.css', 'src/styles/theme.css']) {
    const css = readFileSync(join(root, file), 'utf8');
    const decls = css.match(/(?<![-\w])[a-z][a-z-]*\s*:[^;{}]*;/g) ?? [];
    let previous = '';
    for (const decl of decls) {
      const modern = /color-mix\(|\d(?:\.\d+)?dvh/.test(decl);
      if (modern) {
        const prop = decl.slice(0, decl.indexOf(':')).trim();
        const fallback = previous.slice(0, previous.indexOf(':')).trim();
        assert.equal(
          fallback,
          prop,
          `${file}: \`${decl.trim()}\` has no fallback declaration of \`${prop}\` before it`,
        );
        assert.doesNotMatch(previous, /color-mix\(|\ddvh/, `${file}: the fallback for \`${decl.trim()}\` is itself modern`);
      }
      previous = decl;
    }
  }
});

test('a probe that never answers does not hold the page back', async () => {
  const hanging = ((_url: RequestInfo | URL, init?: RequestInit) =>
    new Promise<Response>((_resolve, reject) => {
      init?.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
    })) as typeof fetch;
  const started = Date.now();
  assert.equal(await signedIn(hanging), true);
  assert.ok(Date.now() - started < 5000);
});
