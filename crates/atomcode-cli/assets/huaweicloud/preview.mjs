// Keep authentication values inside the MCP process, out of model arguments/results.
import { randomUUID } from 'node:crypto';

export const PREVIEW_TOOL = {
  name: 'huaweicloud_sandbox_preview',
  description: 'Expose a running sandbox web port publicly and verify its URL. Requires approval: authenticates remote DevBridge with locally configured Huawei Cloud AK/SK (stored by the remote tool). Returns no keys or raw terminal output. The app and DevBridge must already be installed in the sandbox. Does not support STS session credentials.',
  inputSchema: { type: 'object', required: ['workspace_id', 'port'], additionalProperties: false,
    properties: { workspace_id: { type: 'string' }, port: { type: 'integer', minimum: 1, maximum: 65535 } } },
  annotations: { readOnlyHint: false },
};
const quote = value => "'" + value.replaceAll("'", "'\"'\"'") + "'";
export function createPreview(load = async () => {
  const { resolveCredentialsWithRuntime } = await import('./vendor/src/auth/credentials.mjs');
  const { execOneShot } = await import('./vendor/src/sandbox/session-manager.mjs');
  return { credentials: () => resolveCredentialsWithRuntime({ allowMissing: true }) || {},
    execute: (workspace, command) => execOneShot(workspace, command, 'root', 90000),
    verify: url => fetch(url, { redirect: 'manual', signal: AbortSignal.timeout(10000) }),
  };
}) {
  return async args => {
    if (typeof args.workspace_id !== 'string' || !args.workspace_id.trim() || args.workspace_id.length > 256 || !Number.isInteger(args.port) || args.port < 1 || args.port > 65535) {
      return { ok: false, error: 'A workspace_id and port in 1..65535 are required.' };
    }
    try {
      const deps = await load();
      const credentials = deps.credentials();
      if (!credentials.ak || !credentials.sk) return { ok: false, error: 'Configure Huawei Cloud credentials locally, outside chat, then reconnect the MCP server.' };
      if (credentials.securityToken) return { ok: false, error: 'This preview authentication path does not support STS session credentials; the security token cannot be ignored.' };
      const log = `/tmp/atomcode-preview-${args.port}-${randomUUID()}.log`;
      const command = [
        'set +x; set +o history 2>/dev/null || true; HISTFILE=/dev/null',
        'export PATH="$HOME/.huawei/bin:$PATH"',
        'command -v devbridge >/dev/null 2>&1 || exit 10',
        `curl -fsS --max-time 5 -o /dev/null http://127.0.0.1:${args.port}/ || exit 12`,
        `devbridge auth login --huaweicloud --access-key ${quote(credentials.ak)} --secret-key ${quote(credentials.sk)} >/dev/null 2>&1 || exit 11`,
        `nohup devbridge host -p ${args.port} -e 8 >${quote(log)} 2>&1 </dev/null &`,
        `for attempt in $(seq 1 30); do if grep -q 'Tunnel URL:' ${quote(log)}; then cat ${quote(log)}; exit 0; fi; sleep 1; done; exit 13`,
      ].join('\n');
      const result = await deps.execute(args.workspace_id, command);
      // Never return the command, login output, terminal echo, or SDK exception.
      if (result.exitCode !== 0) return { ok: false, error: 'Preview setup failed. Check application health and remote DevBridge installation/authentication.' };
      const match = /Tunnel URL:\s*(https:\/\/[a-zA-Z0-9.-]+\.myhuaweicloud\.com\/?)/.exec(result.stdout || '');
      if (!match) return { ok: false, error: 'No supported public preview URL was returned.' };
      const response = await deps.verify(match[1]);
      return { ok: response.status >= 200 && response.status < 400, url: match[1], http_status: response.status,
        note: 'Temporary public preview. Stop the remote host process when no longer needed; cloud agreement and account limits apply.' };
    } catch {
      return { ok: false, error: 'Could not verify the public preview. Check the sandbox connection and account setup locally.' };
    }
  };
}
