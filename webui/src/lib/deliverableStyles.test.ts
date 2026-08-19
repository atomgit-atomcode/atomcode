import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '../..');
const css = readFileSync(join(root, 'src/styles/app.css'), 'utf8');

test('jump-to-bottom uses a centered zero-height sticky seat above the scroll edge', () => {
  const slot = css.match(/\.jump-to-bottom-slot\s*\{([^}]*)\}/)?.[1] ?? '';
  const button = css.match(/\.jump-to-bottom\s*\{([^}]*)\}/)?.[1] ?? '';
  assert.match(slot, /position:\s*sticky/);
  assert.match(slot, /height:\s*0/);
  assert.match(slot, /justify-content:\s*center/);
  assert.match(button, /margin-top:\s*-36px/);
});

test('artifact filenames truncate without hiding their full-path title target', () => {
  const chip = css.match(/\.turn-artifact-chip\s*\{([^}]*)\}/)?.[1] ?? '';
  assert.match(chip, /text-overflow:\s*ellipsis/);
  assert.match(chip, /white-space:\s*nowrap/);
});
