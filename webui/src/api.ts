// Task 12 — API client for atomcode webui

// Read the one-time token from URL; never persist to localStorage
const token = new URLSearchParams(location.search).get('token') ?? '';

function authHeaders(): Record<string, string> {
  return token ? { Authorization: 'Bearer ' + token } : {};
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
  | { type: 'error'; message: string };

export interface StreamChatBody {
  message: string;
  session_id?: string;
  working_dir?: string;
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
  decision: 'allow' | 'deny' | 'always_allow',
): Promise<{ success: boolean }> {
  const resp = await fetch('/chat/permission', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ session_id: sessionId, decision }),
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

export interface SessionMessage {
  role: string;
  content: string;
  tool_calls?: unknown;
  tool_result?: unknown;
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

// --- Filesystem browsing ---

export interface FsListResult {
  path: string;
  dirs: string[];
}

export async function listDir(path: string): Promise<FsListResult> {
  const resp = await fetch('/fs/list?path=' + encodeURIComponent(path), {
    headers: authHeaders(),
  });
  return resp.json();
}

// --- Change default directory ---

export interface CdResponse {
  success: boolean;
  message: string;
  current_dir: string;
  project_hash: string;
}

export async function setDefaultDir(path: string): Promise<CdResponse> {
  const resp = await fetch('/cd', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ path }),
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
