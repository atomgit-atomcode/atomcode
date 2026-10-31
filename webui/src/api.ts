import { apiFetch } from './lib/transport.ts';

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

/**
 * The body of a response that succeeded, or an `ApiCallError` saying which
 * status it was and what the daemon said.
 *
 * A failed request used to come back as its error body cast to the success
 * type — `{error}` read as a config, a skill list, a directory listing — and
 * failed somewhere downstream with nothing naming the endpoint; an HTML error
 * page from a proxy failed as `Unexpected token '<'`.
 */
async function readJson<T>(r: Response): Promise<T> {
  if (!r.ok) throw await failure(r);
  return r.json() as Promise<T>;
}

/**
 * What a failed response said, as an error to throw.
 *
 * The daemon answers a failure as `{error, code}` on most routes and as a bare
 * JSON string on some (`/sessions/resolve/:id`'s 409 names the buckets an id is
 * duplicated across). Either way that sentence is the part a person can act
 * on, and "HTTP 409 Conflict" alone is the part they cannot. An HTML error
 * page from a proxy is not shown — only its status.
 */
export async function failure(r: Response): Promise<ApiCallError> {
  const status = `HTTP ${r.status}${r.statusText ? ' ' + r.statusText : ''}`;
  const text = await r.text().catch(() => '');
  let said = '';
  let code: string | undefined;
  try {
    const body: unknown = JSON.parse(text);
    if (typeof body === 'string') {
      said = body;
    } else if (body && typeof body === 'object') {
      const e = body as { error?: unknown; code?: unknown };
      if (typeof e.error === 'string') said = e.error;
      if (typeof e.code === 'string') code = e.code;
    }
  } catch {
    if (text && !/^\s*</.test(text)) said = text.trim().slice(0, 500);
  }
  // The daemon's sentence alone when it gave one — that is what the person
  // reads, and the model settings page shows it verbatim; the status stays on
  // `.status` for code that branches on it.
  return new ApiCallError(said || `${status} ${r.url}`, r.status, code);
}

export type SSEEvent =
  | { type: 'runtime_info'; provider: string; model: string }
  | { type: 'session_assigned'; session_id: string }
  | { type: 'text'; content: string }
  | { type: 'reasoning'; content: string }
  | { type: 'tool_start'; id: string; name: string; arguments: unknown }
  | { type: 'tool_output'; chunk: string }
  | { type: 'tool_progress'; id: string; progress: string }
  | { type: 'tool_result'; id: string; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'permission_request'; session_id: string; tool_name: string; reason: string; call_id: string; arguments: unknown; allow_all_bash?: boolean }
  | UserInputRequestEvent
  | { type: 'user_input_resolved'; request_id: number }
  | { type: 'done'; tokens: unknown; tool_calls: unknown; session_id: string; stats?: TurnStats; stop_reason?: string; message?: string }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
  | { type: 'warning'; message: string }
  | { type: 'persistence_warning'; message: string }
  | { type: 'rate_limited'; reset_at_display: string; reset_label: string; secs_until_reset: number | null; auto_resuming: boolean; server_message?: string | null }
  | PolicyInterventionEvent
  // Artifact events: the daemon's ArtifactDetector strips fenced code blocks from
  // TextDelta and emits them as separate artifact_start / artifact_content / artifact_end
  // events (see ArtifactDetector in crates/atomcode-daemon/src/lib.rs). Without handling
  // these, code block content is silently lost in the WebUI while the TUI sees it fine.
  | { type: 'artifact_start'; id: string; artifact_type: string; language?: string | null; title?: string | null }
  | { type: 'artifact_content'; id: string; content: string }
  | { type: 'artifact_end'; id: string }
  | { type: 'command_output'; text: string };

export interface ModelInfo {
  provider: string;
  model: string;
  provider_type: string;
  is_default: boolean;
  /** Whether this concrete model endpoint accepts `reasoning_effort`.
   *  The effort selector is shown only when true. */
  effort_applicable: boolean;
  /** Current effort: 'low' | 'medium' | 'high' | 'max' | null (model default). */
  reasoning_effort: string | null;
  /** The effort LEVELS this endpoint exposes (subset of low/medium/high/max,
   *  canonical order). The selector shows only these; an unrestricted endpoint
   *  lists all four (never empty). */
  effort_levels: string[];
}

export async function getModels(): Promise<ModelInfo[]> {
  const r = await apiFetch('/models', { headers: authHeaders() });
  if (!r.ok) throw new Error(`list models failed: ${r.status}`);
  const body: unknown = await r.json();
  if (!Array.isArray(body)) throw new Error('list models returned an invalid payload');
  return body as ModelInfo[];
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
  approval_mode?: ApprovalMode;
}

export async function stopChat(requestId: string): Promise<void> {
  const resp = await apiFetch('/chat/stop', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ session_id: requestId }),
  });
  if (!resp.ok) throw new Error(`stop chat failed: ${resp.status}`);
}

export async function getActiveChatSessions(): Promise<string[]> {
  const resp = await apiFetch('/chat/active', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`active chats failed: ${resp.status}`);
  const body: unknown = await resp.json();
  if (!Array.isArray(body) || !body.every((entry) => typeof entry === 'string')) {
    throw new Error('active chats returned an invalid payload');
  }
  return body;
}

/**
 * Detach a local `/chat` stream without orphaning its daemon operation.
 *
 * Session switches cannot keep consuming the old SSE response, but aborting
 * fetch alone does not prove the daemon turn stopped. Abort the local reader
 * immediately, then use the existing cancellation endpoint; callers must
 * surface rejection because the old turn may still own the runtime.
 */
export async function cancelDetachedChat(
  requestId: string,
  controller: AbortController,
): Promise<void> {
  controller.abort();
  await stopChat(requestId);
}

export async function streamChat(
  body: StreamChatBody,
  onEvent: (event: SSEEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const resp = await apiFetch('/chat', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify(body),
    signal,
  });

  if (!resp.ok) {
    throw await failure(resp);
  }

  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let terminalSeen = false;

  const emit = (event: SSEEvent) => {
    if (event.type === 'done' || event.type === 'stopped' || event.type === 'error') {
      terminalSeen = true;
    }
    onEvent(event);
  };

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
        emit(parsed);
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
          emit(parsed);
        } catch {
          // Ignore
        }
      }
    }
  }

  if (signal?.aborted) {
    const error = new Error('chat stream aborted');
    error.name = 'AbortError';
    throw error;
  }
  if (!terminalSeen) {
    throw new Error('chat stream ended before an authoritative terminal event');
  }
}

export async function respondPermission(
  sessionId: string,
  decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist' | 'allow_all_bash',
  toolName?: string,
): Promise<{ success: boolean }> {
  const resp = await apiFetch('/chat/permission', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ session_id: sessionId, decision, tool_name: toolName }),
  });
  // A request that did not land throws: the card must stay up to be answered
  // again, or the person thinks it was approved while the turn waits on an
  // answer nobody can give any more. `success: false` is a different thing —
  // the question is already gone (answered elsewhere, or timed out) — and is
  // returned for the caller to close on.
  return readJson<{ success: boolean; error?: string }>(resp);
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
  working_directory_available?: boolean;
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
  synthetic?: boolean;
  internal_origin?: string;
  internalOrigin?: string;
  tool_calls?: ToolCallInfo[];
  tool_result?: ToolResultInfo;
  artifacts?: unknown;
  images?: ImageData[];
  /** Epoch ms for the message's completed turn. Older sessions without a native
   *  transcript omit it; the UI must not replace it with the current wall clock. */
  created_at?: number;
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

// NOTE: `/sessions` caps at the 50 most-recent sessions ACROSS ALL projects.
// For a project's full history use listProjectSessions; for finding a session
// anywhere use searchSessions. This capped list is only for cross-project
// lookups where 50 is enough (e.g. URL-restore of a recent session).
/** Bounded native-only metadata, ranked by project. Timestamps use epoch seconds. */
export async function sessionReferenceCandidates(q: string, cwd: string, signal?: AbortSignal): Promise<SessionMetaWithProject[]> {
  const resp = await apiFetch(`/sessions/references?q=${encodeURIComponent(q)}&cwd=${encodeURIComponent(cwd)}`, {
    headers: authHeaders(), signal,
  });
  return readJson(resp);
}

export async function listSessions(): Promise<SessionMetaWithProject[]> {
  const resp = await apiFetch('/sessions', { headers: authHeaders() });
  return readJson(resp);
}

// A single project's sessions, UNCAPPED (reads one bucket directly). This is
// what the sidebar shows — the global `/sessions` cap would otherwise starve a
// project of its own history when many other projects have newer sessions.
// The endpoint returns bare SessionMeta; every row is in `projectHash`, so we
// stamp it back on for the client's project-scoped dedup/filter.
export async function listProjectSessions(projectHash: string): Promise<SessionMetaWithProject[]> {
  const resp = await apiFetch(`/projects/${encodeURIComponent(projectHash)}/sessions`, {
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`list project sessions failed: ${resp.status}`);
  const list: SessionMeta[] = await resp.json();
  return list.map((m) => ({ ...m, project_hash: projectHash }));
}

// Cross-project session search by name, UNCAPPED. Backs the search modal so it
// can find a session in ANY project (the sidebar list itself is per-project).
export async function searchSessions(q: string, includeUnavailable = false): Promise<SessionMetaWithProject[]> {
  const resp = await apiFetch(`/sessions/search?q=${encodeURIComponent(q)}${includeUnavailable ? "&include_unavailable=true" : ""}`, {
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`search sessions failed: ${resp.status}`);
  return resp.json();
}

// Resolve a (short) session id to its full record across all projects, UNCAPPED.
// URL-restore only has a short id from the address bar; the capped `/sessions`
// can't locate an older session. Returns null when nothing matches.
export async function resolveSession(id: string): Promise<SessionMetaWithProject | null> {
  const resp = await apiFetch(`/sessions/resolve/${encodeURIComponent(id)}`, {
    headers: authHeaders(),
  });
  if (resp.status === 404) return null;
  if (!resp.ok) throw await failure(resp);
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
  const resp = await apiFetch('/sessions', {
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
  const resp = await apiFetch(
    `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(sessionId)}/rename`,
    {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify({ name }),
    },
  );
  if (!resp.ok) throw new Error(`rename failed: ${resp.status}`);
}

export class DeleteSessionError extends Error {
  readonly code?: string;

  constructor(message: string, code?: string) {
    super(message);
    this.name = 'DeleteSessionError';
    this.code = code;
  }
}

export async function deleteSession(
  projectHash: string,
  sessionId: string,
): Promise<void> {
  const resp = await apiFetch(
    `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(sessionId)}`,
    { method: 'DELETE', headers: authHeaders() },
  );
  if (!resp.ok) {
    const payload: unknown = await resp.json().catch(() => undefined);
    const errorValue =
      payload && typeof payload === 'object' && 'error' in payload
        ? (payload as { error: unknown }).error
        : undefined;
    const codeValue =
      payload && typeof payload === 'object' && 'code' in payload
        ? (payload as { code: unknown }).code
        : undefined;
    const detail =
      typeof payload === 'string'
        ? payload
        : typeof errorValue === 'string'
          ? errorValue
          : undefined;
    const code = typeof codeValue === 'string' ? codeValue : undefined;
    throw new DeleteSessionError(detail || `delete failed: ${resp.status}`, code);
  }
}

// --- Config types ---

export interface ProviderInfo {
  name: string;
  type: string;
  model: string;
  supports_vision: boolean;
  supports_vision_override: boolean | null;
  base_url?: string;
  has_api_key: boolean;
  requires_login?: boolean;
  is_default: boolean;
  context_window?: number;
  /** Whether prior-turn reasoning (思维链) is echoed back to the model:
   * `"preserve"` (echo only turns that had it) / `"exclude"`, or absent/null =
   * auto-detect from the model. (`"include"` — always echo, placeholder when
   * missing — is also accepted but reserved for auto-detected models.) */
  reasoning_history?: string | null;
  max_tokens?: number | null;
  /** The account this model hangs off (missing from an older daemon). */
  account?: string | null;
  /** The model's own display name. */
  display_name?: string | null;
  /** The default reasoning level; absent/null is the endpoint's own default. */
  reasoning_effort?: string | null;
  /** The levels this model declares; absent/null is no declaration. */
  reasoning_effort_levels?: string[] | null;
  /** CodingPlan-managed: shown, never edited (server-decided). */
  managed?: boolean;
}

export interface ProviderAccountInfo {
  id: string;
  provider: string;
  display_name?: string;
  type: string;
  base_url?: string;
  has_api_key: boolean;
  header_names?: string[];
  model_ids: string[];
  legacy: boolean;
  managed: boolean;
  /** What to call it: its own name, else its id, else the vendor's. */
  label?: string;
  /** The vendor or protocol it speaks, by name. */
  preset_name?: string;
  /** A generic protocol pointed at the person's own endpoint. */
  custom?: boolean;
  /** Its models can be listed from the endpoint. */
  discoverable?: boolean;
  /** Its endpoint can be checked after a save. */
  probeable?: boolean;
}

export interface ProviderPresetInfo {
  id: string;
  display_name: string;
  type: string;
  default_base_url?: string;
  requires_api_key: boolean;
  /** Missing when a newer WebUI is temporarily paired with an older daemon. */
  model_source?: 'embedded' | 'discovery_api' | 'manual';
  /** Its models can be listed from the endpoint. */
  discoverable?: boolean;
}

export interface DiscoveredModelInfo {
  id: string;
  supports_vision?: boolean;
  reasoning_effort_levels?: string[];
  reasoning_effort?: string;
  name?: string;
  context_window?: number;
  max_tokens?: number;
}

export interface NotificationConfigInfo {
  enabled: boolean;
  min_duration_secs: number;
  bell: boolean;
}

export interface ConfigInfo {
  path: string;
  default_provider: string;
  default_workdir?: string;
  providers: ProviderInfo[];
  /** Reusable connection identities. API keys are intentionally never returned. */
  provider_accounts?: ProviderAccountInfo[];
  /** Compiled provider catalog shared with the TUI add-provider flow. */
  provider_presets?: ProviderPresetInfo[];
  /** Generic protocols for a custom endpoint (OpenAI-compatible, …). */
  provider_protocols?: ProviderPresetInfo[];
  /** Every reasoning level this build knows, in canonical order. */
  reasoning_effort_levels?: string[];
  /** 完成通知配置；旧 daemon 未暴露时为 undefined，前端回退默认值。 */
  notifications?: NotificationConfigInfo;
}

export async function getConfig(): Promise<ConfigInfo> {
  const resp = await apiFetch('/config', { headers: authHeaders() });
  return readJson(resp);
}

/** Trigger a hot-reload of config from disk (POST /config/reload). */
export interface ConfigReloadResult extends ConfigInfo {
  live_provider_sync?: {
    status: 'unbound' | 'unchanged' | 'reloaded' | 'pending' | 'invalid' | 'failed';
    provider: string | null;
    message: string | null;
  };
}

export async function postConfigReload(): Promise<ConfigReloadResult> {
  const resp = await apiFetch('/config/reload', {
    method: 'POST',
    headers: authHeaders(),
  });
  return readJson(resp);
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
  const resp = await apiFetch('/projects', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`list projects failed: ${resp.status}`);
  const body: unknown = await resp.json();
  if (!Array.isArray(body)) throw new Error('list projects returned an invalid payload');
  return body as ProjectInfo[];
}

// --- Current project state ---

export interface ProjectState {
  working_dir: string;
  previous_dir?: string;
  recent_dirs?: string[];
  name?: string;
  // Physical session-bucket hash for `working_dir`. The sidebar scopes its
  // list by this (not the mutable `working_dir` string) so sessions whose
  // stored working_dir was restamped can't leak across projects.
  project_hash?: string;
}

export async function getProject(): Promise<ProjectState> {
  const resp = await apiFetch('/project', { headers: authHeaders() });
  return readJson(resp);
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
  /** Whether the current project is trusted for MCP. Absent on older daemons — treat as untrusted. */
  trusted?: boolean;
  /** Names of MCP servers withheld because the project is untrusted. Absent on older daemons — treat as empty. */
  blocked?: string[];
}

export async function getMcpStatus(): Promise<McpStatusInfo> {
  const resp = await apiFetch('/mcp/status', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`mcp status failed: ${resp.status}`);
  return resp.json();
}

/** Trust the current project for MCP servers, then rebuild the MCP registry. */
export async function postLiveMcpTrust(): Promise<{ ok: boolean; error?: string }> {
  const resp = await apiFetch('/live/mcp/trust', {
    method: 'POST',
    headers: authHeaders(),
  });
  return resp.json();
}

// --- User-invocable skills (for the input "+" attach menu) ---

export interface SkillInfo {
  name: string;
  description: string;
}

export async function getSkills(): Promise<SkillInfo[]> {
  const resp = await apiFetch('/skills', { headers: authHeaders() });
  return readJson(resp);
}

// --- Remote access (relay or local network) status ---

export interface PgyInfo {
  installed: boolean;
  ipv4: string | null;
}

export interface TunnelStatus {
  relay_start_supported?: boolean;
  mode?: 'relay' | 'lan';
  relay_error?: string | null;
  bind_host: string;
  port: number;
  /** Remote access is available through a relay or a non-loopback listener. */
  reachable: boolean;
  pgy: PgyInfo;
  /** Authenticated phone URL; relay preferred, LAN as an explicit alternative. */
  remote_url: string | null;
  /** SVG string of the QR code for remote_url; null when not usable */
  qr_svg: string | null;
}

export async function getTunnelStatus(): Promise<TunnelStatus> {
  const resp = await apiFetch('/tunnel/status', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`tunnel status failed: ${resp.status}`);
  return resp.json();
}

/** Explicitly enable remote access through the current terminal host. */
export async function startTunnelRelay(signal?: AbortSignal): Promise<TunnelStatus> {
  const resp = await apiFetch('/tunnel/start', {
    method: 'POST', headers: { ...authHeaders(), 'Content-Type': 'application/json' },
    body: '{}', signal,
  });
  if (!resp.ok) {
    const body = await resp.json().catch(() => null);
    throw new Error(body?.error ?? `relay start failed: ${resp.status}`);
  }
  return getTunnelStatus();
}

// --- Provider CRUD ---

export async function discoverProviderModels(body: {
  headers?: Record<string, string | null>;
  type: string;
  base_url: string;
  api_key?: string;
  provider_name?: string;
}): Promise<DiscoveredModelInfo[]> {
  const r = await apiFetch('/providers/discover-models', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!r.ok) {
    const e = await r.json().catch(() => ({}));
    throw new Error((e as { error?: string }).error || `HTTP ${r.status}`);
  }
  const payload = await r.json() as { models?: unknown };
  if (!Array.isArray(payload.models)) throw new Error('model discovery returned an invalid payload');
  return payload.models as DiscoveredModelInfo[];
}

// --- Provider accounts (account → models), written by the shared provider book ---

/** A refusal from the daemon: `code` names it so the page can say it in the
 * person's language; `message` is the daemon's own words. */
export class ApiCallError extends Error {
  code?: string;
  status: number;
  constructor(message: string, status: number, code?: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await apiFetch(path, {
    method,
    headers: body === undefined ? authHeaders() : { 'Content-Type': 'application/json', ...authHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return readJson<T>(r);
}

const enc = encodeURIComponent;

export interface NewModelBody {
  model: string;
  display_name?: string;
  context_window?: number;
  max_tokens?: number;
  supports_vision?: boolean;
  reasoning_effort?: string;
  reasoning_effort_levels?: string[];
}

export interface CreateAccountBody {
  headers?: Record<string, string | null>;
  /** Required for a custom protocol; a vendor preset defaults to its own id. */
  id?: string;
  /** A vendor preset id or a protocol id. */
  provider: string;
  display_name?: string;
  base_url?: string;
  api_key?: string;
  models: NewModelBody[];
  set_default?: boolean;
}

/** POST /provider-accounts — an account and its models in one write. */
export function createAccount(body: CreateAccountBody) {
  return call<{ account: string; models: string[]; config: ConfigInfo }>('POST', '/provider-accounts', body);
}

export interface EditAccountBody {
  headers?: Record<string, string | null>;
  /** Empty clears the name. */
  display_name?: string;
  /** Empty falls back to the protocol's own endpoint. */
  base_url?: string;
  /** Empty or omitted keeps the stored key. */
  api_key?: string;
}

/** PATCH /provider-accounts/:id — the connection every model on it shares. */
export function editAccount(id: string, body: EditAccountBody) {
  return call<{ config: ConfigInfo }>('PATCH', `/provider-accounts/${enc(id)}`, body);
}

/** DELETE /provider-accounts/:id — the account and every model on it. */
export function deleteAccount(id: string) {
  return call<{ config: ConfigInfo }>('DELETE', `/provider-accounts/${enc(id)}`);
}

/** POST /provider-accounts/:id/models */
export function addAccountModels(id: string, models: NewModelBody[]) {
  return call<{ created: string[]; config: ConfigInfo }>('POST', `/provider-accounts/${enc(id)}/models`, { models });
}

export interface EditModelBody {
  model?: string;
  /** Empty clears the name. */
  display_name?: string;
  context_window?: number;
  max_tokens?: number;
  clear_max_tokens?: boolean;
  supports_vision?: boolean;
  clear_supports_vision?: boolean;
  reasoning_effort?: string;
  clear_reasoning_effort?: boolean;
  reasoning_effort_levels?: string[];
  clear_reasoning_effort_levels?: boolean;
  /** `preserve` / `exclude` / `include`. */
  reasoning_history?: string;
  clear_reasoning_history?: boolean;
}

/** PATCH /model-profiles/:id — one model's own settings; its account is untouched. */
export function editModelProfile(id: string, body: EditModelBody) {
  return call<{ config: ConfigInfo }>('PATCH', `/model-profiles/${enc(id)}`, body);
}

/** DELETE /model-profiles/:id */
export function deleteModelProfile(id: string) {
  return call<{ config: ConfigInfo }>('DELETE', `/model-profiles/${enc(id)}`);
}

/** POST /model-profiles/:id/default */
export function setDefaultModel(id: string) {
  return call<{ config: ConfigInfo }>('POST', `/model-profiles/${enc(id)}/default`);
}

export type ProbeResult =
  | { probed: false; reason: 'managed' | 'no_endpoint' | 'unsupported_protocol' }
  | {
      probed: true;
      ok: boolean;
      kind: 'reachable' | 'key_rejected' | 'model_missing' | 'wrong_path' | 'unreachable' | 'unexpected';
      message: string;
      /** A base URL that answered, when the saved one is a `/v1` away. */
      fix?: string | null;
    };

/** POST /provider-accounts/:id/probe — one request to the endpoint as saved. */
export function probeAccount(id: string, selection?: string) {
  return call<ProbeResult>('POST', `/provider-accounts/${enc(id)}/probe`, selection ? { selection } : {});
}


// --- Filesystem browsing ---

export interface FsListResult {
  path: string;
  dirs: string[];
  /** Regular files in the directory (webui file picker). */
  files?: string[];
}

export async function listDir(path: string): Promise<FsListResult> {
  const resp = await apiFetch('/fs/list?path=' + encodeURIComponent(path), {
    headers: authHeaders(),
  });
  return readJson(resp);
}

/** One `@`-mention search hit: `path` is relative to the searched dir,
 * forward-slashed; directories end with `/`. */
export interface FsSearchMatch {
  path: string;
  is_dir: boolean;
}

export interface FsSearchResult {
  path: string;
  matches: FsSearchMatch[];
}

/** Recursive, gitignore-aware `@`-mention search under `dir`. `token` is the raw
 * text typed after `@` (may embed a scope like `src/apply`); the daemon splits
 * and matches it with the SAME cross-level substring logic as the CLI popup.
 * Pass an `AbortSignal` so a superseded keystroke's request can be cancelled. */
export async function searchFiles(
  dir: string,
  token: string,
  signal?: AbortSignal,
): Promise<FsSearchResult> {
  const resp = await apiFetch(
    '/fs/search?path=' + encodeURIComponent(dir) + '&q=' + encodeURIComponent(token),
    { headers: authHeaders(), signal },
  );
  return readJson(resp);
}

// --- Create directory ---

export async function mkdir(path: string): Promise<{ path: string }> {
  const r = await apiFetch('/fs/mkdir', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ path }),
  });
  if (!r.ok) { const e = await r.json().catch(() => ({})); throw new Error((e as any).error || `HTTP ${r.status}`); }
  return r.json();
}

/** Open an existing file inside the active session's bound working directory. */
export async function openWorkspaceFile(path: string, sessionId?: string): Promise<void> {
  const r = await apiFetch('/fs/open', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ path, session_id: sessionId }),
  });
  if (!r.ok) {
    const body = await r.json().catch(() => ({}));
    throw new Error((body as { error?: string }).error || `HTTP ${r.status}`);
  }
}

// --- Change working directory ---

export interface CdResponse {
  session_id?: string;
  success: boolean;
  message: string;
  current_dir: string;
  project_hash: string;
}

/** Switch the daemon's current working directory.
 *  Always updates the live project state (so a webui switch survives refresh);
 *  `setDefault` also persists it as the configured default (across restarts). */
export async function changeDir(path: string, setDefault = false): Promise<CdResponse> {
  const resp = await apiFetch('/cd', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ path, set_default: setDefault }),
  });
  return readJson(resp);
}

/** Delete a historical project and its sessions catalog. */
export async function deleteProject(hash: string): Promise<void> {
  const resp = await apiFetch(`/projects/${hash}`, {
    method: 'DELETE',
    headers: authHeaders(),
  });
  if (!resp.ok) {
    throw new Error(`deleteProject failed: ${resp.status}`);
  }
}


// --- Session detail (messages endpoint exists) ---

export async function getSession(
  projectHash: string,
  sessionId: string,
  signal?: AbortSignal,
): Promise<SessionDetail> {
  const resp = await apiFetch(`/projects/${projectHash}/sessions/${sessionId}`, {
    headers: authHeaders(),
    signal,
  });
  if (!resp.ok) {
    throw new Error(`getSession failed with status ${resp.status}`);
  }
  return resp.json();
}

export interface TranscriptStep {
  text: string;
  reasoning: string;
  tools: { id: string; name: string; args: string; result: string | null; is_error: boolean | null }[];
}
export interface TranscriptTurn {
  user_truncated?: boolean;
  started_at?: number;
  ts?: number;
  steps?: TranscriptStep[];
  turn_id: number;
  user: string;
  assistant: string;
  reasoning?: string;
  tools?: { name: string; args: string; result: string; is_error: boolean }[];
  undone: boolean;
}

export async function getSessionTranscript(projectHash: string, sessionId: string, signal?: AbortSignal, detail?: { turnId?: number; turnIds?: number[]; all?: boolean; index?: boolean }): Promise<{ session_id: string; turns: TranscriptTurn[] }> {
  const query = new URLSearchParams();
  if (detail?.turnId != null) query.set('turn_id', String(detail.turnId));
  if (detail?.turnIds) query.set('turn_ids', detail.turnIds.join(','));
  if (detail?.all) query.set('detail', 'true');
  if (detail?.index) query.set('summary', 'true');
  const params = query.toString();
  const suffix = params ? `?${params}` : '';
  const resp = await apiFetch(`/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(sessionId)}/transcript${suffix}`, { headers: authHeaders(), signal });
  if (!resp.ok) throw new Error(`history request failed: ${resp.status}`);
  return resp.json();
}

// --- Live session (multi-tab real-time sync) ---

/** Approval mode: 'build' = interactive approval, 'plan' = read-only exploration,
 *  'bypass' = Auto (auto-approve everything). Mirrors the daemon `ApprovalMode`
 *  while preserving the established wire value. */
export type ApprovalMode = 'build' | 'plan' | 'bypass' | 'accept_edits';

export interface ApprovalModeResponse {
  ok: boolean;
  mode: ApprovalMode;
}

export interface TurnStats {
  duration_ms: number;
  rounds: number;
  tool_calls: number;
  prompt_tokens: number;
  completion_tokens: number;
  cached_tokens: number;
}

export type PolicyRecoveryAction =
  | 'complete_externally'
  | 'skip_step'
  | 'view_safe_instructions'
  | 'end_task';

export interface PolicyInterventionEvent {
  type: 'policy_intervention';
  intervention_id: number;
  code: string;
  actions: PolicyRecoveryAction[];
}

export interface GoalRecoveryProgress {
  kind: 'provider' | 'evaluator';
  attempt: number;
  max_attempts: number;
  retry_in_secs: number;
}
export interface LiveGoalProgress {
  active: boolean;
  phase: string;
  last_reason: string | null;
  recovery?: GoalRecoveryProgress | null;
}

export type LiveWireEvent =
  | { type: 'snapshot'; messages: SessionMessage[]; session_id: string; project_hash: string; provider: string; model?: string | null; mode: ApprovalMode; goal?: LiveGoalProgress | null }
  | ({ type: 'goal_changed' } & LiveGoalProgress)
  | { type: 'provider'; provider: string; model?: string | null }
  | { type: 'model'; model: string | null }
  | { type: 'reasoning_effort'; provider: string; effort: string | null; applicable: boolean }
  | { type: 'mode'; mode: ApprovalMode }
  | { type: 'user'; text: string; images?: ImageData[]; client_input_id?: string }
  | { type: 'text'; content: string }
  | { type: 'reasoning'; content: string }
  | { type: 'tool_start'; id: string; name: string; arguments: string }
  | { type: 'tool_output'; chunk: string }
  | { type: 'tool_progress'; id: string; progress: string }
  | { type: 'tool_result'; id: string; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'state'; running: boolean; stop_reason?: string; message?: string; stats?: TurnStats }
  | { type: 'error'; message: string }
  | { type: 'warning'; message: string }
  | { type: 'persistence_warning'; message: string }
  | { type: 'rate_limited'; reset_at_display: string; reset_label: string; secs_until_reset: number | null; auto_resuming: boolean; server_message?: string | null }
  | { type: 'permission_request'; tool_name: string; reason: string; call_id: string; arguments: string; allow_all_bash?: boolean }
  | { type: 'user_input_request'; request_id: number; header: string; question: string; mode: 'single' | 'multiple' | 'text'; options: { label: string; description?: string }[] }
  | { type: 'user_input_resolved'; request_id: number }
  | PolicyInterventionEvent
  | { type: 'policy_intervention_resolved'; intervention_id: number; action: PolicyRecoveryAction }
  | { type: 'policy_intervention_cleared'; intervention_id: number }
  | { type: 'steered'; count: number; inputs: { text: string; images: ImageData[] }[]; client_input_ids: Array<string | null> }
  | { type: 'session_switched'; session_id: string }
  | { type: 'session_renamed'; session_id: string; name: string }
  | { type: 'working_dir'; working_dir: string }
  | { type: 'command_output'; text: string };

export async function streamLive(
  onEvent: (e: LiveWireEvent) => void,
  signal?: AbortSignal,
  sessionId?: string | null,
  // Called on every chunk received (events AND the 15s keepalive ping) so the
  // caller can run a staleness watchdog that reconnects a silently-dead stream.
  onActivity?: () => void,
): Promise<void> {
  const params = sessionId ? `?session_id=${encodeURIComponent(sessionId)}` : '';
  const resp = await apiFetch(`/live${params}`, { headers: authHeaders(), signal });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    onActivity?.();
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
  clientInputId?: string,
): Promise<{
  disposition: 'started' | 'steered';
  generation: number;
  turn_id: number;
  provider?: string;
  providerChangeApplied?: boolean;
}> {
  const resp = await apiFetch('/live/message', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({
      message,
      ...(images && images.length ? { images } : {}),
      ...(provider ? { provider } : {}),
      ...(sessionId ? { session_id: sessionId } : {}),
      ...(clientInputId ? { client_input_id: clientInputId } : {}),
    }),
  });
  if (!resp.ok) throw new Error(`send live message failed: ${resp.status}`);
  const body = await resp.json() as {
    accepted?: boolean;
    disposition?: 'started' | 'steered';
    generation?: number;
    turn_id?: number;
    provider?: string;
    provider_change_applied?: boolean;
    error?: string;
  };
  if (!body.accepted) throw new Error(body.error ?? 'live runtime rejected the message');
  // Compatibility with a daemon from before typed submit receipts. The old
  // response only said `accepted:true`; treating it as started avoids rolling
  // back an input the server has already accepted and duplicating it on retry.
  if (body.disposition === undefined) {
    return { disposition: 'started', generation: 0, turn_id: 0 };
  }
  if (
    (body.disposition !== 'started' && body.disposition !== 'steered') ||
    typeof body.generation !== 'number' ||
    typeof body.turn_id !== 'number'
  ) {
    throw new Error('live runtime returned an invalid submit receipt');
  }
  return {
    disposition: body.disposition,
    generation: body.generation,
    turn_id: body.turn_id,
    ...(typeof body.provider === 'string' ? { provider: body.provider } : {}),
    ...(typeof body.provider_change_applied === 'boolean'
      ? { providerChangeApplied: body.provider_change_applied }
      : {}),
  };
}

export async function postLiveStop(): Promise<void> {
  const resp = await apiFetch('/live/stop', {
    method: 'POST',
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`stop live chat failed: ${resp.status}`);
  const body = await resp.json() as { accepted?: boolean };
  if (!body.accepted) throw new Error('live runtime rejected the stop request');
}

/** Sync-mode manual compaction: dispatch a compaction against the shared live
 *  runtime. `accepted:false` means no live runtime is bound (nothing to compact). */
export async function postLiveCompact(): Promise<{ accepted: boolean }> {
  const resp = await apiFetch('/live/compact', {
    method: 'POST',
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`live compact failed: ${resp.status}`);
  const body = await resp.json() as { accepted?: boolean };
  return { accepted: body.accepted === true };
}

/** Ask the bound native runtime to resume an existing session. */
export async function postLiveSwitchSession(
  sessionId: string,
): Promise<{
  ok: boolean;
  activeTurn: boolean;
  error?: string;
  /** Where the runtime is now: a copy's own id when `forkedFrom` is set. */
  sessionId?: string;
  /** The session asked for, when another atomcode held it and a copy was opened. */
  forkedFrom?: string;
}> {
  const resp = await apiFetch('/live/switch_session', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ session_id: sessionId }),
  });
  if (!resp.ok) throw new Error(`switch live session failed: ${resp.status}`);
  const body = await resp.json() as {
    ok?: boolean;
    active_turn?: boolean;
    error?: string;
    session_id?: string | null;
    forked_from?: string | null;
  };
  return {
    ok: body.ok === true,
    activeTurn: body.active_turn === true,
    error: body.error,
    ...(body.session_id ? { sessionId: body.session_id } : {}),
    ...(body.forked_from ? { forkedFrom: body.forked_from } : {}),
  };
}

/** Sync-mode model switch: notify the daemon immediately when the dropdown
 *  changes (not just on send), so the TUI header and other tabs follow. */
export async function postLiveProvider(
  provider: string,
  sessionId?: string | null,
): Promise<{ ok: boolean; activeTurn: boolean; error?: string }> {
  const resp = await apiFetch('/live/provider', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ provider, ...(sessionId ? { session_id: sessionId } : {}) }),
  });
  if (!resp.ok) throw new Error(`switch live provider failed: ${resp.status}`);
  // A business rejection (e.g. active_turn) is NOT thrown — the caller reverts its
  // optimistic selection and shows a notice. Only transport failures throw.
  const body = await resp.json() as { ok?: boolean; active_turn?: boolean; error?: string };
  return { ok: body.ok === true, activeTurn: body.active_turn === true, error: body.error };
}

// --- /command endpoint ---

export type CommandResult =
  | { kind: 'undo'; undone: number }
  | { kind: 'remember'; scope: 'global' | 'project' | 'local'; text?: string }
  | { kind: 'memory_action'; text: string }
  | { kind: 'forget'; removed: string[] }
  | { kind: 'memory'; global: string[]; project: string[]; local?: string[]; text?: string }
  | { kind: 'context'; used_tokens: number; total_messages: number; ctx_window: number; utilization: number; ctx_name: string }
  | { kind: 'compact'; applied: boolean; removed_messages: number; before_tokens: number; after_tokens: number }
  | { kind: 'whoami'; logged_in: boolean; username?: string; name?: string; email?: string }
  | { kind: 'status'; logged_in: boolean; username?: string; provider: string; model: string; working_dir: string; config_path: string; text: string }
  | { kind: 'config'; path: string; provider: string }
  | { kind: 'diff'; stat: string }
  | { kind: 'cost'; total_tokens: number; turn_count: number }
  | { kind: 'todo'; items: { status: string; content: string }[] }
  | { kind: 'error'; message: string };

export async function postCommand(body: {
  command: string;
  arg?: string;
  session_id?: string;
  working_dir?: string;
  project_hash?: string;
  provider?: string;
}): Promise<CommandResult> {
  const resp = await apiFetch('/command', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!resp.ok) throw new Error(`command failed: ${resp.status}`);
  return resp.json();
}

/** Switch the approval mode (build / accept_edits / bypass / plan). Runtime
 *  session state — the next turn's PermissionDecider follows it; broadcast to
 *  other tabs. */
export async function postLiveMode(mode: ApprovalMode): Promise<ApprovalMode> {
  const resp = await apiFetch('/approval_mode', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ mode }),
  });
  if (!resp.ok) throw new Error(`switch mode failed: ${resp.status}`);
  const body = (await resp.json()) as ApprovalModeResponse;
  if (!body.ok) throw new Error('live runtime rejected the mode switch');
  return body.mode;
}

export async function getApprovalMode(): Promise<ApprovalMode> {
  const resp = await apiFetch('/approval_mode', { headers: authHeaders() });
  if (!resp.ok) throw new Error(`get mode failed: ${resp.status}`);
  const body = (await resp.json()) as ApprovalModeResponse;
  return body.mode;
}

export class ReasoningEffortError extends Error {
  readonly activeTurn: boolean;
  constructor(message: string, activeTurn = false) {
    super(message);
    this.activeTurn = activeTurn;
  }
}

/** Set `reasoning_effort` for a provider endpoint. `effort` is
 *  'low' | 'medium' | 'high' | 'xhigh' | 'max' | null (clear → model default).
 *  Persists to the provider config so the next turn (live or /chat) picks it up. */
export async function postLiveReasoningEffort(
  effort: string | null,
  provider?: string,
): Promise<void> {
  const resp = await apiFetch('/live/reasoning_effort', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({
      reasoning_effort: effort,
      ...(provider ? { provider } : {}),
    }),
  });
  const body = await resp.json().catch(() => ({})) as { ok?: boolean; error?: string; active_turn?: boolean };
  if (!resp.ok || !body.ok) {
    throw new ReasoningEffortError(body.error ?? `set live reasoning effort failed: ${resp.status}`, body.active_turn === true);
  }
}

export async function postLivePermission(
  decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist' | 'allow_all_bash',
  toolName?: string,
): Promise<{ accepted: boolean }> {
  const resp = await apiFetch('/live/permission', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ decision, tool_name: toolName }),
  });
  if (!resp.ok) throw new Error(`answer live permission failed: ${resp.status}`);
  return resp.json();
}

export interface UserInputQuestion {
  header: string;
  question: string;
  mode: 'single' | 'multiple' | 'text';
  options: { label: string; description?: string }[];
  /** Offer the "type your own answer" row (single/multiple). Absent ⇒ true. */
  custom?: boolean;
}

export interface UserInputResponseBody {
  declined: boolean;
  selected: string[];
  text: string | null;
}

export interface UserInputRequestEvent {
  type: 'user_input_request';
  request_id: number;
  /** The tool continued; this question may still be answered in the live runtime. */
  goal_question_pending?: boolean;
  /** Session correlation. Session-scoped `/live` events may also carry this field. */
  session_id?: string;
  header: string;
  question: string;
  mode: 'single' | 'multiple' | 'text';
  options: { label: string; description?: string }[];
  /// Present for a multi-question batch; the webui steps through these and posts
  /// one batched answer. Omitted for a single question (use the flat fields above).
  questions?: UserInputQuestion[];
  /// Offer the "type your own answer" row for a single question. Absent ⇒ true.
  custom?: boolean;
}

/** A structured-input wire event after Chat records which transport emitted it. */
export type RoutedUserInputRequest = UserInputRequestEvent & {
  response_transport: 'chat' | 'live';
};

export function isUserInputBatch(req: UserInputRequestEvent): boolean {
  return Array.isArray(req.questions) && req.questions.length > 0;
}

export type UserInputAnswer =
  | ({ request_id: number } & UserInputResponseBody)
  | { request_id: number; responses: UserInputResponseBody[] };

export async function postLiveUserInput(
  body: UserInputAnswer,
): Promise<{ accepted: boolean }> {
  const resp = await apiFetch('/live/user-input', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!resp.ok) throw new Error(`answer live user input failed: ${resp.status}`);
  const result = await resp.json() as { accepted: boolean; error?: string };
  if (!result.accepted) {
    throw new Error(result.error ?? 'live runtime did not accept the user input answer');
  }
  return result;
}

export async function postLivePolicyInterventionResolution(
  interventionId: number,
  action: Exclude<PolicyRecoveryAction, 'view_safe_instructions'>,
): Promise<{ accepted: boolean }> {
  const resp = await apiFetch('/live/policy-intervention', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ intervention_id: interventionId, action }),
  });
  if (!resp.ok) throw new Error(`resolve live policy intervention failed: ${resp.status}`);
  const result = await resp.json() as { accepted: boolean; error?: string };
  if (!result.accepted) {
    throw new Error(result.error ?? 'live runtime did not accept the policy intervention resolution');
  }
  return result;
}

export async function postChatUserInput(
  sessionId: string,
  body: UserInputAnswer,
): Promise<{ accepted: boolean }> {
  const resp = await apiFetch('/chat/user-input', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ session_id: sessionId, ...body }),
  });
  if (!resp.ok) throw new Error(`answer chat user input failed: ${resp.status}`);
  const result = await resp.json() as { accepted: boolean; error?: string };
  if (!result.accepted) {
    throw new Error(result.error ?? 'chat runtime did not accept the user input answer');
  }
  return result;
}

/**
 * Answer a structured-input request through the transport that emitted it.
 * A scoped `/live` event also has a `session_id`, so endpoint selection must
 * use the explicit client-side transport marker rather than field presence.
 */
export function postUserInputAnswer(
  req: RoutedUserInputRequest,
  body: UserInputAnswer,
): Promise<{ accepted: boolean }> {
  if (req.response_transport === 'chat') {
    if (!req.session_id) {
      return Promise.reject(new Error('chat user input request is missing session_id'));
    }
    return postChatUserInput(req.session_id, body);
  }
  return postLiveUserInput(body);
}

// --- Git panel (source control) ---
//
// The daemon asks git on every call and caches nothing, so a refresh is just
// another `gitStatus()`. There is no second repository store on this side.

/** One changed path. `status` is git's two-character porcelain code. */
export interface GitChange {
  path: string;
  status: string;
  staged: boolean;
  unstaged: boolean;
  conflicted: boolean;
  untracked: boolean;
  /** Source path of a rename / copy. */
  orig_path?: string;
}

/** A configured remote. Never carries userinfo. */
export interface GitRemote {
  name: string;
  url: string;
}

export interface GitStatus {
  repo: boolean;
  root?: string;
  branch: string;
  upstream: string;
  ahead: number;
  behind: number;
  has_commits: boolean;
  changes: GitChange[];
  remotes: GitRemote[];
  /** Present when `repo` is false: why (not a repository / git unavailable). */
  error?: string;
}

export type GitDiffKind = 'staged' | 'worktree' | 'untracked' | 'conflict';

export interface GitDiffResult {
  path: string;
  kind: GitDiffKind;
  patch: string;
  binary: boolean;
  truncated: boolean;
}

export interface GitCommandResult {
  output: string;
}

export interface GitInitResult extends GitCommandResult {
  root: string;
}

export interface GitCommitResult extends GitCommandResult {
  commit: string;
}

export interface GitPushResult extends GitCommandResult {
  upstream: string;
}

export interface GitCloneResult extends GitCommandResult {
  dir: string;
  root: string;
}

/** A stored credential. The token itself is never returned. */
export interface GitCredential {
  host: string;
  username: string;
}

async function postJson<T>(path: string, body?: unknown): Promise<T> {
  const resp = await apiFetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return readJson<T>(resp);
}

export async function gitStatus(): Promise<GitStatus> {
  const resp = await apiFetch('/git/status', { headers: authHeaders() });
  return readJson<GitStatus>(resp);
}

export async function gitDiff(path: string, kind: GitDiffKind): Promise<GitDiffResult> {
  const resp = await apiFetch(`/git/diff?path=${encodeURIComponent(path)}&kind=${kind}`, {
    headers: authHeaders(),
  });
  return readJson<GitDiffResult>(resp);
}

export function gitInit(): Promise<GitInitResult> {
  return postJson<GitInitResult>('/git/init');
}

export function gitStage(paths: string[]): Promise<GitCommandResult> {
  return postJson<GitCommandResult>('/git/stage', { paths });
}

export function gitUnstage(paths: string[]): Promise<GitCommandResult> {
  return postJson<GitCommandResult>('/git/unstage', { paths });
}

export function gitCommit(message: string): Promise<GitCommitResult> {
  return postJson<GitCommitResult>('/git/commit', { message });
}

export function gitPull(remote?: string, branch?: string): Promise<GitCommandResult> {
  return postJson<GitCommandResult>('/git/pull', { remote, branch });
}

export function gitPush(remote?: string, branch?: string): Promise<GitPushResult> {
  return postJson<GitPushResult>('/git/push', { remote, branch });
}

export function gitClone(url: string, dir?: string): Promise<GitCloneResult> {
  return postJson<GitCloneResult>('/git/clone', { url, dir });
}

export async function listGitCredentials(): Promise<GitCredential[]> {
  const resp = await apiFetch('/git/credentials', { headers: authHeaders() });
  const body = await readJson<{ credentials?: GitCredential[] }>(resp);
  return body.credentials ?? [];
}

export function storeGitCredential(
  host: string,
  username: string,
  token: string,
): Promise<{ success: boolean; host: string }> {
  return postJson<{ success: boolean; host: string }>('/git/credentials', {
    host,
    username,
    token,
  });
}

export async function removeGitCredential(host: string): Promise<boolean> {
  const resp = await apiFetch(`/git/credentials/${encodeURIComponent(host)}`, {
    method: 'DELETE',
    headers: authHeaders(),
  });
  const body = await readJson<{ removed?: boolean }>(resp);
  return body.removed === true;
}

/** A runtime package installed on this device — an interpreter or toolchain
 *  that is not in the APK. Android forbids executing such a package directly,
 *  so it ships as a library and runs through `pluginhost`. */
export interface PackageRecord {
  name: string;
  version: string;
  description: string;
  /** Library inside the package that the launcher loads. */
  entry: string;
  symbol: string;
  /** Command names it answers to: its own name plus aliases. */
  commands: string[];
  /** The version kept beside this one; present means a rollback is possible. */
  previous_version?: string;
  size_bytes: number;
}

export async function listPackages(): Promise<PackageRecord[]> {
  const resp = await apiFetch('/packages', { headers: authHeaders() });
  const body = await readJson<{ packages?: PackageRecord[] }>(resp);
  return body.packages ?? [];
}

/** Install from a URL or a local bundle path. `sha256` is verified when given. */
export async function installPackage(source: {
  url?: string;
  path?: string;
  sha256?: string;
}): Promise<PackageRecord> {
  const body = await postJson<{ package: PackageRecord }>('/packages/install', source);
  return body.package;
}

export async function removePackage(name: string): Promise<boolean> {
  const resp = await apiFetch(`/packages/${encodeURIComponent(name)}`, {
    method: 'DELETE',
    headers: authHeaders(),
  });
  const body = await readJson<{ removed?: boolean }>(resp);
  return body.removed === true;
}

/** Whether the optional helper APK (`com.atomcode.tools`) is installed.
 *
 *  It is a second APK that carries tools runtime packages cannot host — `make`
 *  spawns a compiler, and Android forbids executing anything from this app's own
 *  data directory. The tools show up on `PATH` only when that APK is present, so
 *  the panel surfaces it instead of leaving `make` unexplained. */
export interface HelperStatus {
  installed: boolean;
  /** Command names it contributed (`make`, `cc`, `tcc`). */
  tools: string[];
}

export async function listHelperStatus(): Promise<HelperStatus> {
  const resp = await apiFetch('/packages/helper', { headers: authHeaders() });
  const body = await readJson<{ installed?: boolean; tools?: string[] }>(resp);
  return { installed: body.installed === true, tools: body.tools ?? [] };
}

/** A provider APK as the market repository publishes it.
 *
 *  It is **not** a plugin: it is a second app carrying binaries that have to
 *  spawn children or run as a program (`make` runs a compiler, `zig` *is* the
 *  program), and Android forbids executing anything from this app's own data
 *  directory. Publishing it is what lets the panel offer a download instead of
 *  only an `adb install` line.
 *
 *  There is more than one: the tools helper holds `make`/`tcc`/the CLI, and zig
 *  alone is 141 MB and ships in its own APK. Go, clang and the rest would follow
 *  the same route, so the market publishes a list. */
export interface MarketHelper {
  name: string;
  /** Android package id (`com.atomcode.tools.zig`) — what install state is keyed on. */
  package?: string;
  title: string;
  summary: string;
  version: string;
  size_bytes: number;
  sha256: string;
  download_url?: string | null;
  download_path?: string | null;
}

/**
 * Download one provider APK into this app's storage.
 *
 * The daemon fetches it from the same base the plugins come from and checks the
 * published hash, then writes it where the app can find it. The returned path
 * is inside the app's own storage — installing it is still the person's
 * decision, which is why this only prepares the file.
 *
 * `name` is the provider's manifest id (or its Android package id). Omit it for
 * the first one, which is what a market with a single provider means.
 */
export async function downloadHelperApk(
  name?: string,
): Promise<{ path: string; version: string; packageId: string | null }> {
  const query = name ? `?name=${encodeURIComponent(name)}` : '';
  const resp = await apiFetch(`/packages/market/helper${query}`, { headers: authHeaders() });
  const body = await readJson<{ path?: string; version?: string; package?: string }>(resp);
  if (!body.path) throw new Error('the daemon did not return a provider file');
  return { path: body.path, version: body.version ?? '', packageId: body.package ?? null };
}

/**
 * Swap back to the version kept beside the installed one.
 *
 * An upgrade can install cleanly and still be wrong to run, so the replaced
 * version is kept for exactly this. The swap also makes the *current* version
 * the new previous one, so a rollback can be undone by rolling back again.
 */
export async function rollbackPackage(name: string): Promise<PackageRecord> {
  const body = await postJson<{ package: PackageRecord }>(
    `/packages/${encodeURIComponent(name)}/rollback`,
  );
  return body.package;
}

/** Whether an installed package would actually run, and why not when it would not. */
export interface PackageHealth {
  name: string;
  ok: boolean;
  /** The version when healthy, the loader's complaint when broken. */
  detail: string;
}

/**
 * Probe every installed package through the launcher.
 *
 * An install can succeed and still be useless — a wrong-architecture `.so`, a
 * missing symbol, a library that needs something the phone does not have. This
 * is how the panel finds out without the person running it in a shell first.
 */
export async function listPackageHealth(): Promise<PackageHealth[]> {
  const resp = await apiFetch('/packages/health', { headers: authHeaders() });
  const body = await readJson<{ packages?: PackageHealth[] }>(resp);
  return body.packages ?? [];
}

/** A valid package bundle found on the device's local storage. */
export interface LocalPackage {
  path: string;
  name: string;
  version: string;
  description: string;
  size_bytes: number;
}

export type PackageDiscoveryEvent =
  | { type: 'progress'; directories: number; archives: number; skipped: number }
  | { type: 'found'; package: LocalPackage }
  | {
      type: 'done';
      directories: number;
      archives: number;
      skipped: number;
      truncated: boolean;
    }
  | { type: 'error'; message: string };

/**
 * Scan local storage for valid runtime-package bundles.
 *
 * Results arrive one at a time because a full phone-storage scan can be slow.
 * Aborting the signal closes the stream and stops the daemon's directory walk.
 */
export async function discoverLocalPackages(
  onEvent: (event: PackageDiscoveryEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const resp = await apiFetch('/packages/discover/stream', {
    headers: authHeaders(),
    signal,
  });
  if (!resp.ok) throw await failure(resp);
  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let finished = false;
  let streamError: string | null = null;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const parts = buffer.split('\n\n');
    buffer = parts.pop() ?? '';
    for (const part of parts) {
      const line = part.split('\n').find((candidate) => candidate.startsWith('data:'));
      if (!line) continue;
      const json = line.slice('data:'.length).trim();
      if (!json) continue;
      let event: PackageDiscoveryEvent;
      try {
        event = JSON.parse(json) as PackageDiscoveryEvent;
      } catch {
        continue;
      }
      onEvent(event);
      if (event.type === 'done') finished = true;
      if (event.type === 'error') streamError = event.message;
    }
  }

  if (streamError) throw new Error(streamError);
  if (!finished) throw new Error('package discovery stream ended without a result');
}

/** One event on the install progress stream. */
export interface PackageProgress {
  type: 'progress' | 'done' | 'error';
  /** Which step is running: fetch the bytes, check the hash, unpack, link. */
  stage?: 'download' | 'verify' | 'unpack' | 'link';
  received?: number;
  total?: number;
  files?: number;
  message?: string;
  package?: PackageRecord;
}

/**
 * Install while reporting progress.
 *
 * A 47 MB bundle takes long enough that a motionless button reads as a hang, so
 * the panel subscribes to the daemon's event stream instead of awaiting one
 * response. The stream always ends with exactly one `done` or `error`.
 */
export async function installPackageStreaming(
  source: { url?: string; path?: string; sha256?: string },
  onEvent: (event: PackageProgress) => void,
  signal?: AbortSignal,
): Promise<PackageRecord> {
  const resp = await apiFetch('/packages/install/stream', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(source),
    signal,
  });
  if (!resp.ok) {
    throw await failure(resp);
  }

  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let installed: PackageRecord | null = null;
  let streamError: string | null = null;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    // SSE frames are separated by a blank line; the tail may be incomplete.
    const parts = buffer.split('\n\n');
    buffer = parts.pop() ?? '';
    for (const part of parts) {
      const line = part.split('\n').find((candidate) => candidate.startsWith('data:'));
      if (!line) continue;
      const json = line.slice('data:'.length).trim();
      if (!json) continue;
      let event: PackageProgress;
      try {
        event = JSON.parse(json) as PackageProgress;
      } catch {
        continue; // keepalive comment or partial frame
      }
      onEvent(event);
      if (event.type === 'done' && event.package) installed = event.package;
      if (event.type === 'error') streamError = event.message ?? 'install failed';
    }
  }

  if (streamError) throw new Error(streamError);
  if (!installed) throw new Error('install stream ended without a result');
  return installed;
}

/** Runtime data that outlived its package. */
export interface RuntimeLeftover {
  name: string;
  path: string;
  bytes: number;
}

export interface LeftoverReport {
  leftovers: RuntimeLeftover[];
  totalBytes: number;
}

/** What an uninstalled runtime left behind (`python-packages`, `.local`).
 *
 *  The daemon reports these only when no interpreter is installed any more —
 *  while one is, those directories are live and worth their space. */
export async function listLeftovers(): Promise<LeftoverReport> {
  const resp = await apiFetch('/packages/leftovers', { headers: authHeaders() });
  const body = await readJson<{ leftovers?: RuntimeLeftover[]; total_bytes?: number }>(resp);
  return { leftovers: body.leftovers ?? [], totalBytes: body.total_bytes ?? 0 };
}

/** Delete those leftovers. Returns the bytes actually reclaimed. */
export async function cleanLeftovers(): Promise<number> {
  const resp = await apiFetch('/packages/leftovers', {
    method: 'DELETE',
    headers: authHeaders(),
  });
  const body = await readJson<{ freed_bytes?: number }>(resp);
  return body.freed_bytes ?? 0;
}

/** One sidebar section of the plugin market. */
export interface MarketCategory {
  id: string;
  title: string;
  blurb?: string;
}

/** One plugin as the market repository describes it.
 *
 *  `download_url` and `download_path` are resolved by the daemon from the
 *  entry's `artifact` field, so this page never has to know whether the bundle
 *  lives in Git LFS, a release asset or an object store. */
/** 市场里的一个具体版本（可装回旧版）。 */
export interface MarketPluginVersion {
  version: string;
  size_bytes: number;
  sha256: string;
  download_url?: string | null;
  download_path?: string | null;
}

export interface MarketPlugin {
  name: string;
  title: string;
  summary: string;
  category: string;
  tags: string[];
  version: string;
  size_bytes: number;
  sha256: string;
  artifact_kind?: string;
  download_url?: string | null;
  download_path?: string | null;
  /** 全部可装版本，最新的在最后；老仓库可能没有这一项。 */
  versions?: MarketPluginVersion[];
  installed: boolean;
  installed_version?: string | null;
}

export interface MarketIndex {
  success: boolean;
  error?: string;
  hint?: string;
  base?: string;
  storage?: string;
  generated_at?: string;
  from_cache?: boolean;
  categories: MarketCategory[];
  plugins: MarketPlugin[];
  /** The first provider APK, when the repository publishes one (old servers). */
  helper?: MarketHelper | null;
  /** Every provider APK the repository publishes, in publication order. */
  helpers?: MarketHelper[];
}

/** The market index, from the separate plugin repository.
 *
 *  `refresh` skips the daemon's cache — the person pressed the refresh button. */
export async function listMarket(refresh = false): Promise<MarketIndex> {
  const query = refresh ? '?refresh=1' : '';
  const resp = await apiFetch(`/packages/market${query}`, { headers: authHeaders() });
  const body = await readJson<Partial<MarketIndex>>(resp);
  return {
    success: body.success ?? false,
    error: body.error,
    hint: body.hint,
    base: body.base,
    storage: body.storage,
    generated_at: body.generated_at,
    from_cache: body.from_cache,
    categories: body.categories ?? [],
    plugins: body.plugins ?? [],
    helper: body.helper ?? null,
    helpers: body.helpers ?? (body.helper ? [body.helper] : []),
  };
}

/** Point the market at another repository (or a local directory).
 *
 *  Written into `config.toml`'s `[market] base` by the daemon, so it survives a
 *  restart — that is the whole reason this is not just frontend state. */
export async function setMarketBase(base: string): Promise<{ success: boolean; error?: string }> {
  const resp = await apiFetch('/packages/market/config', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ base }),
  });
  const body = await readJson<{ success?: boolean; error?: string }>(resp);
  return { success: body.success ?? false, error: body.error };
}
