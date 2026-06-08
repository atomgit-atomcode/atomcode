import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

export function expandHome(p) {
  if (typeof p === 'string' && p.startsWith('~/')) return join(homedir(), p.slice(2));
  return p;
}

export function loadConfig() {
  const here = dirname(fileURLToPath(import.meta.url));
  const path = join(here, '..', 'config.json');
  const cfg = JSON.parse(readFileSync(path, 'utf8'));
  cfg.tokenPath = expandHome(cfg.tokenPath);
  cfg.workingDir = expandHome(cfg.workingDir);
  return cfg;
}
