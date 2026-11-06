import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { homedir } from 'node:os';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

export const SUPPORTED_AGENT_TARGETS = [
  'opencode',
  'codex',
  'codex-desktop',
  'codearts',
  'workbuddy',
  'dsh',
  'officeace',
  'hermes',
  'openclaw',
  'atomcode',
];

function baseHome() {
  return process.env.HUAWEICLOUD_HOME || homedir();
}

function opencodeConfigFile() {
  const jsonc = join(baseHome(), '.config', 'opencode', 'opencode.jsonc');
  if (existsSync(jsonc)) return jsonc;
  return join(baseHome(), '.config', 'opencode', 'opencode.json');
}

function readJsonSafe(path) {
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

function opencodeRegistered() {
  const path = opencodeConfigFile();
  const cfg = readJsonSafe(path);
  return Boolean(cfg?.mcp?.['huaweicloud-devkit']);
}

function codexDesktopRegistered() {
  const path = join(baseHome(), '.codex', 'config.toml');
  if (!existsSync(path)) return false;
  try {
    return readFileSync(path, 'utf8').includes('[mcp_servers.huaweicloud-devkit]');
  } catch {
    return false;
  }
}

function codexCliRegistered() {
  try {
    const r = spawnSync('codex', ['plugin', 'list'], {
      shell: false,
      windowsHide: true,
      stdio: 'pipe',
      timeout: 10000,
    });
    const out = `${r.stdout || ''}${r.stderr || ''}`;
    return out.includes('huaweicloud-core');
  } catch {
    return false;
  }
}

function codeartsRegistered() {
  const paths = [
    join(baseHome(), '.codeartsdoer', 'mcp', 'mcp_settings.json'),
    join(process.cwd(), '.codeartsdoer', 'mcp', 'mcp_settings.json'),
  ];
  return paths.some((path) => {
    const cfg = readJsonSafe(path);
    return Boolean(cfg?.mcpServers?.['huaweicloud-devkit']);
  });
}

function workbuddyRegistered() {
  const cfg = readJsonSafe(join(baseHome(), '.workbuddy', 'mcp.json'));
  return Boolean(cfg?.mcpServers?.['huaweicloud-devkit']);
}

function dshRoot() {
  return process.env.DSH_HOME || join(baseHome(), '.dsh');
}

function dshRegistered() {
  const patchPath = join(dshRoot(), 'profiles', 'web', 'cordis.patch.yml');
  if (!existsSync(patchPath)) return false;
  try {
    const patch = readFileSync(patchPath, 'utf8');
    return (
      patch.includes('id: mcp-huaweicloud') &&
      patch.includes('@deepseek-ai/dsh-mcp-client') &&
      patch.includes('serverName: huaweicloud')
    );
  } catch {
    return false;
  }
}

function readOfficeaceRegistryInstallDir() {
  if (process.platform !== 'win32') return null;
  try {
    const r = spawnSync('reg', ['query', 'HKCU\\SOFTWARE\\OfficeAce\\OfficeAce', '/v', 'InstallDir'], {
      encoding: 'utf8',
      windowsHide: true,
      timeout: 5000,
    });
    if (r.status === 0) {
      const m = r.stdout.match(/InstallDir\s+REG_SZ\s+(.+)/);
      if (m) return m[1].trim();
    }
  } catch {}
  return null;
}

function officeaceCapabilitiesDir() {
  const configRoot = process.env.OFFICE_CLAW_CONFIG_ROOT;
  if (configRoot && existsSync(join(configRoot, 'capabilities.json'))) return configRoot;
  const regDir = readOfficeaceRegistryInstallDir();
  if (regDir) {
    const dir = join(regDir, '.office-claw');
    if (existsSync(join(dir, 'capabilities.json'))) return dir;
  }
  if (process.platform === 'win32') {
    const bases = [process.env.ProgramFiles, 'C:\\Program Files', 'D:\\Program Files'];
    if (process.env.LOCALAPPDATA) bases.push(join(process.env.LOCALAPPDATA, 'Programs'));
    for (const base of bases) {
      if (!base) continue;
      const dir = join(base, 'OfficeAce', '.office-claw');
      if (existsSync(join(dir, 'capabilities.json'))) return dir;
    }
  }
  return null;
}

function officeaceCapabilitiesDirSafe() {
  return officeaceCapabilitiesDir() || join(baseHome(), '.office-claw');
}

function officeaceSqlitePath() {
  const capDir = officeaceCapabilitiesDirSafe();
  return join(resolve(capDir, '..'), 'data', 'mcp-connectors.sqlite');
}

function officeaceRegistered() {
  let hasMcp = false;
  const dbPath = officeaceSqlitePath();
  if (existsSync(dbPath)) {
    const nodeMajor = Number(process.versions.node.split('.')[0]);
    if (nodeMajor >= 22) {
      try {
        const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
        const db = new DatabaseSync(dbPath, { readonly: true });
        const row = db.prepare("SELECT enabled FROM mcp_connectors WHERE name = 'huaweicloud-devkit'").get();
        db.close();
        hasMcp = Boolean(row?.enabled);
      } catch {}
    }
  }

  const capFile = join(officeaceCapabilitiesDirSafe(), 'capabilities.json');
  const cfg = readJsonSafe(capFile);
  const hasSkills = cfg?.capabilities?.some((c) => c.id === 'huaweicloud-core' && c.type === 'skill') ?? false;

  return hasMcp || hasSkills;
}

function atomcodeHome() {
  return process.env.ATOMCODE_HOME || join(baseHome(), '.atomcode');
}

function atomcodeRegistered() {
  const cfg = readJsonSafe(join(atomcodeHome(), 'mcp.json'));
  return Boolean(cfg?.mcpServers?.['huaweicloud-devkit']);
}

function hermesHome() {
  if (process.env.HERMES_HOME) return process.env.HERMES_HOME;
  // Hermes on Windows stores under LOCALAPPDATA, not ~/.hermes
  if (process.platform === 'win32' && process.env.LOCALAPPDATA) {
    return join(process.env.LOCALAPPDATA, 'hermes');
  }
  return join(baseHome(), '.hermes');
}

function hermesRegistered() {
  const configPath = join(hermesHome(), 'config.yaml');
  if (!existsSync(configPath)) return false;
  try {
    const content = readFileSync(configPath, 'utf8');
    return content.includes('mcp_servers:') && content.includes('huaweicloud-devkit');
  } catch {
    return false;
  }
}

export function getAgentRegistrationStatuses(target = 'all') {
  const requested = target === 'all' ? SUPPORTED_AGENT_TARGETS : [target];
  const result = { target, agents: {} };
  for (const agent of requested) {
    let configured = false;
    if (agent === 'opencode') configured = opencodeRegistered();
    if (agent === 'codex-desktop') configured = codexDesktopRegistered();
    if (agent === 'codex') configured = codexCliRegistered();
    if (agent === 'codearts') configured = codeartsRegistered();
    if (agent === 'workbuddy') configured = workbuddyRegistered();
    if (agent === 'dsh') configured = dshRegistered();
    if (agent === 'officeace') configured = officeaceRegistered();
    if (agent === 'hermes') configured = hermesRegistered();
    if (agent === 'atomcode') configured = atomcodeRegistered();
    result.agents[agent] = { configured };
  }
  return result;
}
