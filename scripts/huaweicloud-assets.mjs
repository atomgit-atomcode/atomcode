// Regenerate/check the compile-time asset index after a reviewed upstream update.
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '../crates/atomcode-cli/assets/huaweicloud');
function files(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
    if (['node_modules', 'files.rs', 'adapter.test.mjs', '.DS_Store'].includes(entry.name)) return [];
    const path = join(dir, entry.name);
    if (entry.isSymbolicLink()) throw Error('Bundled assets must not contain symlinks');
    return entry.isDirectory() ? files(path) : [relative(root, path).replaceAll('\\', '/')];
  });
}
const text = 'const FILES: &[(&str, &[u8])] = &[\n' + files(root).sort().map(path => `    (${JSON.stringify(path)}, include_bytes!(${JSON.stringify(path)})),\n`).join('') + '];\n';
const target = join(root, 'files.rs');
if (process.argv.includes('--check')) {
  if (readFileSync(target, 'utf8') !== text) throw Error('Huawei Cloud asset index is stale; run node scripts/huaweicloud-assets.mjs');
  console.log('Huawei Cloud bundled asset index: OK');
} else writeFileSync(target, text);
