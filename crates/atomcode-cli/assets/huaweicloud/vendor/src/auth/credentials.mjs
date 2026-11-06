import { chmodSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';

// Verify a credential file ended up with 0600. On Windows-mounted drives inside WSL
// (drvfs/9p) chmod is silently ignored, so the file can be world-readable (0777).
// Native Windows has no POSIX modes (statSync always reports 0666), so skip the check there.
function ensurePrivateMode(path) {
  if (process.platform === 'win32') return;
  try {
    chmodSync(path, 0o600);
  } catch {}
  try {
    const mode = statSync(path).mode & 0o777;
    if (mode !== 0o600) {
      console.warn(
        `\x1b[33m[WARN]\x1b[0m Could not set 0600 on ${path} (current mode ${mode.toString(8)}). Credentials may be readable by other users.`,
      );
      console.warn(`\x1b[33m       If running under WSL, move the credential home to the Linux filesystem:\x1b[0m`);
      console.warn(`\x1b[33m         export HUAWEICLOUD_HOME=$HOME  (then re-run auth init)\x1b[0m`);
      console.warn(
        `\x1b[33m       Or skip file storage entirely with HW_ACCESS_KEY/HW_SECRET_KEY environment variables.\x1b[0m`,
      );
    }
  } catch {}
}

function baseHome() {
  return process.env.HUAWEICLOUD_HOME || homedir();
}

export function globalCredentialsPath() {
  return join(baseHome(), '.config', 'huaweicloud', 'credentials.json');
}

export function obsConfigPath() {
  return join(baseHome(), '.obsutilconfig');
}

export function readGlobalCredentials() {
  const path = globalCredentialsPath();
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function writeGlobalCredentials(credentials = {}) {
  const path = globalCredentialsPath();
  mkdirSync(dirname(path), { recursive: true });
  const payload = {
    ak: String(credentials.ak || ''),
    sk: String(credentials.sk || ''),
    securityToken: String(credentials.securityToken || ''),
    region: String(credentials.region || ''),
  };
  writeFileSync(path, JSON.stringify(payload, null, 2), { encoding: 'utf8', mode: 0o600 });
  ensurePrivateMode(path);
  return path;
}

export function writeObsConfig(credentials = {}) {
  const region = String(credentials.region || '');
  const ak = String(credentials.ak || '');
  const sk = String(credentials.sk || '');
  const securityToken = String(credentials.securityToken || '');
  if (!region || !ak || !sk) {
    throw new Error('region, ak, and sk are required to write OBS config');
  }
  const path = obsConfigPath();
  const endpoint = credentials.endpoint || `https://obs.${region}.myhuaweicloud.com`;
  // Flat key=value format (no [default] section) as written by KooCLI 7.x `hcloud OBS config`.
  const content = `endpoint=${endpoint}\nak=${ak}\nsk=${sk}${securityToken ? `\ntoken=${securityToken}` : ''}\n`;
  writeFileSync(path, content, { encoding: 'utf8', mode: 0o600 });
  ensurePrivateMode(path);
  return { path, endpoint };
}

export function resolveCredentials(options = {}) {
  let ak = process.env.HW_ACCESS_KEY;
  let sk = process.env.HW_SECRET_KEY;
  let securityToken = process.env.HW_SECURITY_TOKEN;
  let region = process.env.HW_REGION || process.env.HUAWEICLOUD_REGION || '';

  const codeartsCreds = isCodeArtsContext() ? readCodeArtsCredentials() : null;
  if (codeartsCreds) {
    if (!ak && codeartsCreds.ak) ak = codeartsCreds.ak;
    if (!sk && codeartsCreds.sk) sk = codeartsCreds.sk;
    if (!region && codeartsCreds.region) region = codeartsCreds.region;
  }

  const stored = readGlobalCredentials();
  if (stored) {
    if (!ak && stored.ak) ak = stored.ak;
    if (!sk && stored.sk) sk = stored.sk;
    if (!securityToken && stored.securityToken) securityToken = stored.securityToken;
    if (!region && stored.region) region = stored.region;
  }

  if (!ak || !sk) {
    if (options.allowMissing) return null;
    throw new Error(
      'Huawei Cloud credentials are not configured. Run "npx huaweicloud-devkit auth init" or set HW_ACCESS_KEY/HW_SECRET_KEY.',
    );
  }

  return { ak, sk, securityToken, region };
}

function isCodeArtsContext() {
  return existsSync(join(process.cwd(), '.codeartsdoer')) || existsSync(join(homedir(), '.codeartsdoer'));
}

function readCodeArtsCredentials() {
  const searchPaths = [
    join(process.cwd(), '.codeartsdoer', 'mcp', 'mcp_settings.json'),
    join(homedir(), '.codeartsdoer', 'mcp', 'mcp_settings.json'),
  ];

  for (const path of searchPaths) {
    try {
      if (!existsSync(path)) continue;
      const config = JSON.parse(readFileSync(path, 'utf8'));
      const server = config?.mcpServers?.['huaweicloud-devkit'];
      if (!server?.env) continue;

      const ak = server.env.HW_ACCESS_KEY;
      const sk = server.env.HW_SECRET_KEY;
      if (ak && sk) {
        return {
          ak,
          sk,
          securityToken: server.env.HW_SECURITY_TOKEN || '',
          region: server.env.HW_REGION || server.env.HUAWEICLOUD_REGION || '',
        };
      }
    } catch {
      // mcp_settings.json missing or invalid — skip
    }
  }

  return null;
}

let runtimeCredentials = null;

export function setRuntimeCredentials(ak, sk, securityToken, region) {
  runtimeCredentials = { ak, sk, securityToken: securityToken || '', region: region || '' };
}

export function clearRuntimeCredentials() {
  runtimeCredentials = null;
}

export function resolveCredentialsWithRuntime(options = {}) {
  if (runtimeCredentials) {
    return {
      ak: runtimeCredentials.ak,
      sk: runtimeCredentials.sk,
      securityToken: runtimeCredentials.securityToken,
      region: runtimeCredentials.region,
    };
  }

  return resolveCredentials(options);
}
