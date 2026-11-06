import { existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';

import { getAgentRegistrationStatuses } from './agent-registration.mjs';
import { globalCredentialsPath, obsConfigPath, readGlobalCredentials, writeObsConfig } from './credentials.mjs';

function hcloudInstalled() {
  const bin = process.env.HCLOUD_BIN || 'hcloud';
  try {
    const r = spawnSync(`"${bin}" version`, [], {
      shell: true,
      windowsHide: true,
      stdio: 'pipe',
      timeout: 5000,
    });
    const out = `${r.stdout || ''}${r.stderr || ''}`;
    return r.status === 0 && /KooCLI|Current.*version|当前KooCLI/i.test(out);
  } catch {
    return false;
  }
}

export function getAuthStatus(target = 'all') {
  const credentials = readGlobalCredentials();
  return {
    target,
    credentialsConfigured: Boolean(credentials?.ak && credentials?.sk),
    credentialsPath: globalCredentialsPath(),
    obsConfigured: existsSync(obsConfigPath()),
    obsConfigPath: obsConfigPath(),
    kooCliInstalled: hcloudInstalled(),
    agents: getAgentRegistrationStatuses(target).agents,
  };
}

export function syncAuth(target = 'all') {
  const credentials = readGlobalCredentials();
  if (!credentials?.ak || !credentials?.sk) {
    return {
      ok: false,
      error: 'Global credentials are not configured.',
      nextStep: 'Run "npx huaweicloud-devkit auth init" first.',
    };
  }

  let obs;
  try {
    obs = writeObsConfig(credentials);
  } catch (error) {
    return {
      ok: false,
      error: error.message,
      nextStep: 'Run "npx huaweicloud-devkit auth init" to refresh credentials and region.',
    };
  }

  return {
    ok: true,
    obs: { configured: true, path: obs.path, endpoint: obs.endpoint },
    credentialsConfigured: true,
    agents: getAgentRegistrationStatuses(target).agents,
    note: 'OBS credentials were synced from the global credential vault. Agent MCP registration is managed by "npx huaweicloud-devkit install --target <agent>".',
  };
}
