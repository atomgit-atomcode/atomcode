import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { woff2FontSources } from './fontAssets.ts';

test('all KaTeX WOFF2 faces survive while legacy font assets are not referenced', () => {
  const css = readFileSync(new URL('../../node_modules/katex/dist/katex.min.css', import.meta.url), 'utf8');
  const before = [...css.matchAll(/url\(([^)]*\.woff2)\)/g)].map(match => match[1]);
  assert(before.length > 10);
  const optimized = woff2FontSources(css);
  assert.deepEqual([...optimized.matchAll(/url\(([^)]*\.woff2)\)/g)].map(match => match[1]), before);
  assert(!/\.woff["')]|\.ttf["')]/.test(optimized));
  assert.equal((optimized.match(/@font-face/g) ?? []).length, (css.match(/@font-face/g) ?? []).length);
});

test('single-format and unrelated CSS are preserved and quoted URLs are supported', () => {
  assert.equal(woff2FontSources('src:url("a.woff2") format("woff2"),url(a.ttf) format("truetype");'), 'src:url("a.woff2") format("woff2");');
  const css = 'src:url(a.ttf) format("truetype");.x{color:red}';
  assert.equal(woff2FontSources(css), css);
});

test('Source Serif keeps unicode ranges, weights and WOFF2 faces', () => {
  for (const weight of [400, 500]) {
    const css = readFileSync(new URL(`../../node_modules/@fontsource/source-serif-4/${weight}.css`, import.meta.url), 'utf8');
    const optimized = woff2FontSources(css);
    assert(!/\.woff["')]/.test(optimized));
    for (const marker of ['unicode-range:', 'font-weight:', 'font-family:', '.woff2']) {
      assert.equal(optimized.split(marker).length, css.split(marker).length);
    }
  }
});
