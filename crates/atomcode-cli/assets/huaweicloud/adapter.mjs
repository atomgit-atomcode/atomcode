#!/usr/bin/env node
// AtomCode V1 boundary: a restricted catalog, with native MCP approval retained.
import { pathToFileURL } from 'node:url';
import { realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import { PREVIEW_TOOL, createPreview } from './preview.mjs';

const READ_ONLY = new Set([
  'huaweicloud_check_cli', 'huaweicloud_plan_cli_command', 'huaweicloud_list_operations',
  'huaweicloud_run_readonly_command', 'huaweicloud_show_profile_redacted',
  'huaweicloud_hook_check_command', 'huaweicloud_hook_check_artifacts',
  'huaweicloud_hook_check_deploy_plan', 'huaweicloud_service_catalog',
  'huaweicloud_explain_error', 'huaweicloud_search_docs', 'huaweicloud_retrieve_skill',
  'huaweicloud_list_regions', 'huaweicloud_get_regional_availability',
  'huaweicloud_auth_status', 'huaweicloud_sandbox_check_user',
]);
const WRITES = new Set([
  'huaweicloud_sandbox_sign_agreement', 'huaweicloud_sandbox_connect',
  'huaweicloud_sandbox_exec_with_session', 'huaweicloud_sandbox_exec_one_shot',
  'huaweicloud_sandbox_close_session', 'huaweicloud_sandbox_upload_file',
  'huaweicloud_sandbox_upload_project', 'huaweicloud_sandbox_preview',
]);
const SCOPED = new Set([
  'huaweicloud_sandbox_exec_with_session', 'huaweicloud_sandbox_exec_one_shot',
  'huaweicloud_sandbox_close_session', 'huaweicloud_sandbox_upload_file',
  'huaweicloud_sandbox_upload_project', 'huaweicloud_sandbox_preview',
]);
const SECRET_KEY = /^(ak|sk)$|access.*key|secret.*key|security.*token|authorization|password|passwd|credential|private.*key|(^|[_-])token$/i;
export function redact(value, secrets = []) {
  if (Array.isArray(value)) return value.map(entry => redact(entry, secrets));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(
    ([key, entry]) => [key, SECRET_KEY.test(key) && typeof entry !== 'boolean' ? '<redacted>' : redact(entry, secrets)],
  ));
  if (typeof value !== 'string') return value;
  let text = value.replace(/((?:access[_-]?key|secret[_-]?key|security[_-]?token|password|authorization|\bAK|\bSK)\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;]+)/gi, '$1<redacted>');
  for (const secret of secrets) { if (typeof secret === 'string' && secret) text = text.split(secret).join('<redacted>'); }
  for (const key of ['HW_ACCESS_KEY', 'HW_SECRET_KEY', 'HW_SECURITY_TOKEN']) {
    const secret = process.env[key];
    if (secret) text = text.split(secret).join('<redacted>');
  }
  return text;
}

async function loadUpstream() {
  const tools = await import('./vendor/src/tools.mjs');
  return { ...tools, privateSecrets: async () => {
    const { resolveCredentialsWithRuntime } = await import('./vendor/src/auth/credentials.mjs');
    const credentials = resolveCredentialsWithRuntime({ allowMissing: true });
    return credentials ? [credentials.ak, credentials.sk, credentials.securityToken] : [];
  } };
}
export function createDispatch(load = loadUpstream, preview = createPreview()) {
  return async (method, params = {}) => {
    if (method === 'initialize') return {
      protocolVersion: params.protocolVersion || '2024-11-05',
      capabilities: { tools: {} }, serverInfo: { name: 'atomcode-huaweicloud', version: '1.0.0' },
    };
    if (method === 'ping') return {};
    if (method === 'tools/list') {
      const { TOOL_DEFINITIONS } = await load();
      return { tools: [...TOOL_DEFINITIONS, PREVIEW_TOOL].filter(tool => READ_ONLY.has(tool.name) || WRITES.has(tool.name)).map(tool => ({
        ...tool,
        inputSchema: SCOPED.has(tool.name) ? { ...tool.inputSchema, required: [...new Set([...(tool.inputSchema.required || []), 'workspace_id'])] } : tool.inputSchema,
        annotations: { readOnlyHint: READ_ONLY.has(tool.name) },
      })) };
    }
    if (method !== 'tools/call') throw new Error('Unsupported MCP method');
    if (!READ_ONLY.has(params.name) && !WRITES.has(params.name)) throw new Error('Tool is not available in the AtomCode Huawei Cloud V1 integration');
    let secrets = [];
    try {
      if (SCOPED.has(params.name) && (typeof params.arguments?.workspace_id !== 'string' || !params.arguments.workspace_id.trim())) {
        throw new Error('An explicit workspace_id is required; do not rely on another session’s last connection.');
      }
      if (params.name === PREVIEW_TOOL.name) {
        const result = await preview(params.arguments || {});
        return { content: [{ type: 'text', text: JSON.stringify(result) }], isError: !result.ok };
      }
      const upstream = await load();
      if (WRITES.has(params.name) || params.name === 'huaweicloud_run_readonly_command') secrets = await upstream.privateSecrets?.() || [];
      const { callTool } = upstream;
      const args = { ...(params.arguments || {}) };
      if (params.name === 'huaweicloud_retrieve_skill' && !/^[a-z0-9-]+$/.test(args.name || '')) throw new Error('Invalid skill name');
      if (params.name === 'huaweicloud_sandbox_upload_project') {
        args.exclude = [...new Set(['.git', 'node_modules', '.env', '.env.*', '.hcloud', '.huaweicloud', '.aws', '.ssh', '*.pem', '*.key', ...(args.exclude || [])])];
      }
      const result = await callTool(params.name, args);
      if (params.name === 'huaweicloud_auth_status' && result && process.env.HW_ACCESS_KEY && process.env.HW_SECRET_KEY) result.credentialsConfigured = true;
      return { content: [{ type: 'text', text: JSON.stringify(redact(result, secrets)) ?? 'null' }], isError: result?.ok === false || (typeof result?.exitCode === 'number' && result.exitCode !== 0) };
    } catch (error) {
      return { content: [{ type: 'text', text: JSON.stringify(redact({ error: String(error.message || error) }, secrets)) }], isError: true };
    }
  };
}

export function serve(input = process.stdin, output = process.stdout) {
  const dispatch = createDispatch();
  let buffer = Buffer.alloc(0);
  const MAX = 8 * 1024 * 1024;
  const send = (message, framed) => {
    const body = Buffer.from(JSON.stringify(message));
    output.write(framed ? Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]) : Buffer.concat([body, Buffer.from('\n')]));
  };
  const handle = async (body, framed) => {
    let message;
    try { message = JSON.parse(body); } catch {
      send({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Invalid JSON' } }, framed); return;
    }
    if (!message || !Object.hasOwn(message, 'id')) return;
    try { send({ jsonrpc: '2.0', id: message.id, result: await dispatch(message.method, message.params) }, framed); }
    catch { send({ jsonrpc: '2.0', id: message.id, error: { code: -32601, message: 'Unsupported method or unavailable tool' } }, framed); }
  };
  const pending = new Set();
  const enqueue = (body, framed) => {
    const task = handle(body, framed);
    pending.add(task);
    task.finally(() => pending.delete(task));
  };
  input.on('data', chunk => {
    buffer = Buffer.concat([buffer, chunk]);
    if (buffer.length > MAX) { process.stderr.write('Huawei Cloud MCP frame exceeds limit\n'); process.exitCode = 1; input.destroy(); return; }
    while (buffer.length) {
      if (buffer.subarray(0, 15).toString().toLowerCase().startsWith('content-length:')) {
        const end = buffer.indexOf('\r\n\r\n'); if (end < 0) return;
        const match = /Content-Length:\s*(\d+)/i.exec(buffer.subarray(0, end).toString());
        const length = match && Number(match[1]);
        if (!length || length > MAX) { input.destroy(); process.exitCode = 1; return; }
        if (buffer.length < end + 4 + length) return;
        const body = buffer.subarray(end + 4, end + 4 + length).toString();
        buffer = buffer.subarray(end + 4 + length); enqueue(body, true);
      } else {
        // A fragmented Content-Length header must not be mistaken for JSONL.
        if ('content-length:'.startsWith(buffer.toString().toLowerCase())) return;
        const end = buffer.indexOf('\n'); if (end < 0) return;
        const body = buffer.subarray(0, end).toString().trim(); buffer = buffer.subarray(end + 1);
        if (body) enqueue(body, false);
      }
    }
  });
  return new Promise(resolve => {
    const finish = () => Promise.allSettled([...pending]).then(resolve);
    input.on('end', finish);
    input.on('close', finish);
  });
}
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(resolve(process.argv[1]))).href) {
  // Upstream diagnostics must not corrupt the stdio protocol or expose raw errors.
  console.log = () => {};
  console.warn = () => {};
  console.error = () => {};
  await serve();
}
