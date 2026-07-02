// Task 12 — API client for atomcode webui

// Read the one-time token from URL; never persist to localStorage
const token = new URLSearchParams(location.search).get('token') ?? '';

function authHeaders(): Record<string, string> {
  // X-AtomCode-Client lets the daemon tag telemetry as webui-originated
  // (resolve_client_mode → SessionMode::Webui); sent regardless of token.
  const h: Record<string, string> = { 'X-AtomCode-Client': 'webui' };
  if (token) h.Authorization = 'Bearer ' + token;
  return h;
}

/** Current session token (from the page URL). */
export function getToken(): string {
  return token;
}

export type SSEEvent =
  | { type: 'text'; content: string }
  | { type: 'reasoning'; content: string }
  | { type: 'tool_start'; id: string; name: string; arguments: unknown }
  | { type: 'tool_output'; chunk: string }
  | { type: 'tool_result'; id: string; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'permission_request'; session_id: string; tool_name: string; reason: string; call_id: string; arguments: unknown }
  | { type: 'done'; tokens: unknown; tool_calls: unknown; session_id: string }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
  | { type: 'warning'; message: string }
  | { type: 'rate_limited'; reset_at_display: string; reset_label: string; secs_until_reset: number | null; auto_resuming: boolean };

export interface ModelInfo {
  provider: string;
  model: string;
  provider_type: string;
  is_default: boolean;
  /** Whether this model accepts the DeepSeek `reasoning_effort` control
   *  (deepseek-v4 family). The effort selector is shown only when true. */
  effort_applicable: boolean;
  /** Current effort: 'high' | 'max' | null (model default). */
  reasoning_effort: string | null;
}

export async function getModels(): Promise<ModelInfo[]> {
  const r = await fetch('/models', { headers: authHeaders() });
  return r.json();
}

/** A base64-encoded image attachment (no data-URL prefix). */
export interface ImageData {
  media_type: string;
  data: string;
}

export interface StreamChatBody {
  message: string;
  session_id?: string;
  request_id?: string;
  working_dir?: string;
  provider?: string;
  images?: ImageData[];
}

export async function stopChat(requestId: string): Promise<void> {
  const resp = await fetch('/chat/stop', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ session_id: requestId }),
  });
  if (!resp.ok) throw new Error(`stop chat failed: ${resp.status}`);
}

export async function streamChat(
  body: StreamChatBody,
  onEvent: (event: SSEEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const resp = await fetch('/chat', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify(body),
    signal,
  });

  if (!resp.ok) {
    throw new Error(`HTTP ${resp.status} ${resp.statusText}`);
  }

  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });

    // Split on double-newline (SSE event boundaries)
    const parts = buffer.split('\n\n');
    // The last part may be an incomplete event — keep it in buffer
    buffer = parts.pop() ?? '';

    for (const part of parts) {
      // Find the data: line within the event block
      const dataLine = part
        .split('\n')
        .find((line) => line.startsWith('data:'));
      if (!dataLine) continue;

      const jsonStr = dataLine.slice('data:'.length).trim();
      if (!jsonStr) continue;

      try {
        const parsed = JSON.parse(jsonStr) as SSEEvent;
        onEvent(parsed);
      } catch {
        // Ignore malformed lines (keep-alive comments, etc.)
      }
    }
  }

  // Process any trailing content in the buffer
  if (buffer.trim()) {
    const dataLine = buffer
      .split('\n')
      .find((line) => line.startsWith('data:'));
    if (dataLine) {
      const jsonStr = dataLine.slice('data:'.length).trim();
      if (jsonStr) {
        try {
          const parsed = JSON.parse(jsonStr) as SSEEvent;
          onEvent(parsed);
        } catch {
          // Ignore
        }
      }
    }
  }
}

export async function respondPermission(
  sessionId: string,
  decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist',
  toolName?: string,
): Promise<{ success: boolean }> {
  const resp = await fetch('/chat/permission', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ session_id: sessionId, decision, tool_name: toolName }),
  });
  return resp.json();
}

// --- Session types ---

export interface SessionMeta {
  id: string;
  name: string;
  working_dir: string;
  created_at: number;
  updated_at: number;
  message_count: number;
  file_size?: number;
}

export interface SessionMetaWithProject extends SessionMeta {
  project_hash: string;
}

export interface ToolCallInfo {
  id: string;
  name: string;
  arguments: string;
  display: string;
}

export interface ToolResultInfo {
  call_id: string;
  success: boolean;
  summary: string;
  line_count: number;
}

export interface SessionMessage {
  role: 'system' | 'user' | 'assistant' | 'tool';
  content: string;
  tool_calls?: ToolCallInfo[];
  tool_result?: ToolResultInfo;
  artifacts?: unknown;
  images?: ImageData[];
}

export interface SessionDetail {
  id: string;
  name: string;
  working_dir: string;
  created_at: number;
  updated_at: number;
  message_count: number;
  messages: SessionMessage[];
}

export async function listSessions(): Promise<SessionMetaWithProject[]> {
  const resp = await fetch('/sessions', { headers: authHeaders() });
  return resp.json();
}

export interface CreateSessionResponse {
  id: string;
  name: string;
  working_dir: string;
  project_hash: string;
  created_at: number;
}

export async function createSession(
  workingDir?: string,
  title?: string,
  sync?: boolean,
): Promise<CreateSessionResponse> {
  const body: Record<string, string | boolean> = {};
  if (workingDir) body.working_dir = workingDir;
  if (title) body.title = title;
  // 仅在 webui 开启同步时让后端广播会话切换，使 sync 模式 TUI 跟随新建（issue #850）。
  if (sync) body.sync = true;
  const resp = await fetch('/sessions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!resp.ok) throw new Error(`create session failed: ${resp.status}`);
  return resp.json();
}

export async function renameSession(
  projectHash: string,
  sessionId: string,
  name: string,
): Promise<void> {
  const resp = await fetch(
    `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(sessionId)}/rename`,
    {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify({ name }),
    },
  );
  if (!resp.ok) throw new Error(`rename failed: ${resp.status}`);
}

export async function deleteSession(
  projectHash: string,
  sessionId: string,
): Promise<void> {
  const resp = await fetch(
    `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(sessionId)}`,
    { method: 'DELETE', headers: authHeaders() },
  );
  if (!resp.ok) throw new Error(`delete failed: ${resp.status}`);
}

// --- Config types ---

export interface ProviderInfo {
  name: string;
  type: string;
  model: string;
  base_url?: string;
  has_api_key: boolean;
  is_default: boolean;
  context_window?: number;
}

export interface ConfigInfo {
  path: string;
  default_provider: string;
  default_workdir?: string;
  providers: ProviderInfo[];
}

export async function getConfig(): Promise<ConfigInfo> {
  const resp = await fetch('/config', { headers: authHeaders() });
  return resp.json();
}

// --- Projects types ---

export interface ProjectInfo {
  hash: string;
  name: string;
  working_dir: string;
  description?: string;
  session_count: number;
  created_at: number;
  last_updated: number;
}

export async function getProjects(): Promise<ProjectInfo[]> {
  const resp = await fetch('/projects', { headers: authHeaders() });
  return resp.json();
}

// --- Current project state ---

export interface ProjectState {
  working_dir: string;
  previous_dir?: string;
  recent_dirs?: string[];
  name?: string;
}

export async function getProject(): Promise<ProjectState> {
  const resp = await fetch('/project', { headers: authHeaders() });
  return resp.json();
}


// --- MCP server status ---

export interface McpServerInfo {
  name: string;
  status: string;
  tool_count?: number;
  error?: string;
}

export interface McpStatusInfo {
  servers: McpServerInfo[];
}

export async function getMcpStatus(): Promise<McpStatusInfo> {
  const resp = await fetch('/mcp/status', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`mcp status failed: ${resp.status}`);
  return resp.json();
}

// --- User-invocable skills (for the input "+" attach menu) ---

export interface SkillInfo {
  name: string;
  description: string;
}

export async function getSkills(): Promise<SkillInfo[]> {
  const resp = await fetch('/skills', { headers: authHeaders() });
  return resp.json();
}

// --- Remote access (蒲公英 / Oray PGY) status ---

export interface PgyInfo {
  installed: boolean;
  ipv4: string | null;
}

export interface TunnelStatus {
  bind_host: string;
  port: number;
  /** server bound to a non-loopback address (reachable by other devices) */
  reachable: boolean;
  pgy: PgyInfo;
  /** ready-to-use remote URL (蒲公英 ip + token); null when not usable */
  remote_url: string | null;
  /** SVG string of the QR code for remote_url; null when not usable */
  qr_svg: string | null;
}

export async function getTunnelStatus(): Promise<TunnelStatus> {
  const resp = await fetch('/tunnel/status', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`tunnel status failed: ${resp.status}`);
  return resp.json();
}

// --- Provider CRUD ---

export interface CreateProviderBody {
  name: string;
  type: string;       // 'openai' | 'claude' | 'ollama'
  model: string;
  api_key?: string;
  base_url?: string;
  context_window?: number;
  set_default?: boolean;
}

export async function createProvider(body: CreateProviderBody): Promise<unknown> {
  const r = await fetch('/providers', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
  return r.json();
}

export async function deleteProvider(name: string): Promise<void> {
  const r = await fetch(`/providers/${encodeURIComponent(name)}`, { method: 'DELETE', headers: authHeaders() });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
}

export interface UpdateProviderBody {
  // 重命名：传新 name 即把该 provider 改名（后端按 key 迁移并修正默认项）；省略=保持原名。
  name?: string;
  type?: string;
  model?: string;
  // 省略字段=保持不变；传字符串=覆盖。
  api_key?: string;
  base_url?: string;
  context_window?: number;
}

/** PATCH /providers/:name —— 部分更新已有 provider（可改名：body.name 传新名）。 */
export async function updateProvider(name: string, body: UpdateProviderBody): Promise<unknown> {
  const r = await fetch(`/providers/${encodeURIComponent(name)}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
  return r.json();
}

/** POST /providers/:name/default —— 设为默认 provider。 */
export async function setDefaultProvider(name: string): Promise<unknown> {
  const r = await fetch(`/providers/${encodeURIComponent(name)}/default`, {
    method: 'POST',
    headers: authHeaders(),
  });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
  return r.json();
}

// --- Filesystem browsing ---

export interface FsListResult {
  path: string;
  dirs: string[];
  /** Regular files in the directory (webui file picker). */
  files?: string[];
}

export async function listDir(path: string): Promise<FsListResult> {
  const resp = await fetch('/fs/list?path=' + encodeURIComponent(path), {
    headers: authHeaders(),
  });
  return resp.json();
}

// --- Create directory ---

export async function mkdir(path: string): Promise<{ path: string }> {
  const r = await fetch('/fs/mkdir', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ path }),
  });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
  return r.json();
}

// --- Change working directory ---

export interface CdResponse {
  success: boolean;
  message: string;
  current_dir: string;
  project_hash: string;
}

/** Switch the daemon's current working directory.
 *  Always updates the live project state (so a webui switch survives refresh);
 *  `setDefault` also persists it as the configured default (across restarts). */
export async function changeDir(path: string, setDefault = false): Promise<CdResponse> {
  const resp = await fetch('/cd', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ path, set_default: setDefault }),
  });
  return resp.json();
}

// --- Session detail (messages endpoint exists) ---

export async function getSession(
  projectHash: string,
  sessionId: string,
): Promise<SessionDetail> {
  const resp = await fetch(`/projects/${projectHash}/sessions/${sessionId}`, {
    headers: authHeaders(),
  });
  return resp.json();
}

// --- Live session (multi-tab real-time sync) ---

export type LiveWireEvent =
  | { type: 'snapshot'; messages: SessionMessage[]; session_id: string; project_hash: string; provider: string }
  | { type: 'provider'; provider: string }
  | { type: 'user'; text: string; images?: ImageData[] }
  | { type: 'text'; content: string }
  | { type: 'reasoning'; content: string }
  | { type: 'tool_start'; id: string; name: string; arguments: string }
  | { type: 'tool_output'; chunk: string }
  | { type: 'tool_result'; id: string; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'state'; running: boolean }
  | { type: 'error'; message: string }
  | { type: 'warning'; message: string }
  | { type: 'rate_limited'; reset_at_display: string; reset_label: string; secs_until_reset: number | null; auto_resuming: boolean }
  | { type: 'permission_request'; tool_name: string; reason: string; call_id: string; arguments: string }
  | { type: 'session_switched'; session_id: string }
  | { type: 'session_renamed'; session_id: string; name: string }
  | { type: 'working_dir'; working_dir: string };

export async function streamLive(
  onEvent: (e: LiveWireEvent) => void,
  signal?: AbortSignal,
  sessionId?: string | null,
): Promise<void> {
  const params = sessionId ? `?session_id=${encodeURIComponent(sessionId)}` : '';
  const resp = await fetch(`/live${params}`, { headers: authHeaders(), signal });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const parts = buffer.split('\n\n');
    buffer = parts.pop() ?? '';
    for (const part of parts) {
      const line = part.split('\n').find((l) => l.startsWith('data:'));
      if (!line) continue;
      const json = line.slice('data:'.length).trim();
      if (!json) continue;
      try { onEvent(JSON.parse(json) as LiveWireEvent); } catch { /* ignore keepalive */ }
    }
  }
}

export async function postLiveMessage(
  message: string,
  images?: ImageData[],
  provider?: string,
  sessionId?: string | null,
): Promise<void> {
  await fetch('/live/message', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({
      message,
      ...(images && images.length ? { images } : {}),
      ...(provider ? { provider } : {}),
      ...(sessionId ? { session_id: sessionId } : {}),
    }),
  });
}

export async function postLiveStop(): Promise<void> {
  const resp = await fetch('/live/stop', {
    method: 'POST',
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`stop live chat failed: ${resp.status}`);
}

/** Sync-mode session switch: notify the daemon when the user selects a
 *  different (existing) session in the sidebar, so the same-process TUI
 *  follows — loading that session's history. Broadcasts via the same path
 *  as new-session creation; a no-op server-side when no view is attached. */
export async function postLiveSwitchSession(sessionId: string): Promise<void> {
  await fetch('/live/switch_session', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ session_id: sessionId }),
  });
}

/** Sync-mode model switch: notify the daemon immediately when the dropdown
 *  changes (not just on send), so the TUI header and other tabs follow. */
export async function postLiveProvider(provider: string): Promise<void> {
  await fetch('/live/provider', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ provider }),
  });
}

/** Set the DeepSeek V4 `reasoning_effort` for a provider. `effort` is
 *  'high' | 'max' | null (clear → model default). Persists to the provider
 *  config so the next turn (live or /chat) picks it up. */
export async function postLiveReasoningEffort(
  effort: string | null,
  provider?: string,
): Promise<void> {
  await fetch('/live/reasoning_effort', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({
      reasoning_effort: effort,
      ...(provider ? { provider } : {}),
    }),
  });
}

export async function postLivePermission(
  decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist',
  toolName?: string,
): Promise<{ accepted: boolean }> {
  const resp = await fetch('/live/permission', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ decision, tool_name: toolName }),
  });
  return resp.json();
}
