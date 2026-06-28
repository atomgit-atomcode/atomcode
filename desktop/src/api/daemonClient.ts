/**
 * AtomCode Daemon API Client
 *
 * This client communicates with the atomcode-daemon (spawned by Electron main process)
 * for all file operations, AI chat, model management, etc.
 */

// ============================================================================
// Health & Project
// ============================================================================

export async function healthCheck(baseUrl: string): Promise<boolean> {
  try {
    const resp = await fetch(`${baseUrl}/health`);
    return resp.ok;
  } catch {
    return false;
  }
}

export interface ProjectState {
  working_dir: string;
  previous_dir: string;
  recent_dirs: string[];
  name: string;
}

export async function getProject(baseUrl: string): Promise<ProjectState> {
  const resp = await fetch(`${baseUrl}/project`);
  if (!resp.ok) throw new Error(`Failed to get project: ${resp.statusText}`);
  return resp.json();
}

export async function changeDir(baseUrl: string, path: string): Promise<void> {
  const resp = await fetch(`${baseUrl}/cd`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path }),
  });
  if (!resp.ok) throw new Error(`Failed to change directory: ${resp.statusText}`);
}

// ============================================================================
// Filesystem
// ============================================================================

export interface FsTreeEntry {
  name: string;
  path: string;
  type: 'file' | 'directory' | 'limit';
  children?: FsTreeEntry[];
}

export interface FsTreeResponse {
  path: string;
  tree: FsTreeEntry[];
}

export async function getFsTree(baseUrl: string, path: string): Promise<FsTreeResponse> {
  const resp = await fetch(`${baseUrl}/fs/tree?path=${encodeURIComponent(path)}`);
  if (!resp.ok) throw new Error(`Failed to get file tree: ${resp.statusText}`);
  return resp.json();
}

export interface FsListResult {
  path: string;
  dirs: string[];
  files: string[];
}

export async function listDir(baseUrl: string, path: string): Promise<FsListResult> {
  const resp = await fetch(`${baseUrl}/fs/list?path=${encodeURIComponent(path)}`);
  if (!resp.ok) throw new Error(`Failed to list directory: ${resp.statusText}`);
  return resp.json();
}

export interface FileReadResponse {
  path: string;
  content: string;
  language: string;
  size: number;
}

export async function readFile(baseUrl: string, path: string): Promise<FileReadResponse> {
  const resp = await fetch(`${baseUrl}/fs/read?path=${encodeURIComponent(path)}`);
  if (!resp.ok) throw new Error(`Failed to read file: ${resp.statusText}`);
  return resp.json();
}

export async function writeFile(baseUrl: string, path: string, content: string): Promise<void> {
  const resp = await fetch(`${baseUrl}/fs/write`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path, content }),
  });
  if (!resp.ok) throw new Error(`Failed to write file: ${resp.statusText}`);
}

// ============================================================================
// Chat / Streaming
// ============================================================================

export interface StreamChatBody {
  message: string;
  provider?: string;
  working_dir?: string;
  session_id?: string;
}

export type SSEEventType =
  | 'text' | 'reasoning' | 'tool_start' | 'tool_output' | 'tool_result'
  | 'tokens' | 'artifact_start' | 'artifact_content' | 'artifact_end'
  | 'done' | 'stopped' | 'error';

export interface SSEEvent {
  type: SSEEventType;
  data: any;
}

export async function streamChat(
  baseUrl: string,
  body: StreamChatBody,
  onEvent: (event: SSEEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const resp = await fetch(`${baseUrl}/chat`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal,
  });

  if (!resp.ok) {
    const errText = await resp.text();
    onEvent({ type: 'error', data: { message: errText } });
    return;
  }

  const reader = resp.body?.getReader();
  if (!reader) {
    onEvent({ type: 'error', data: { message: 'No response body' } });
    return;
  }

  const decoder = new TextDecoder();
  let buffer = '';

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';

    for (const line of lines) {
      if (line.startsWith('data: ')) {
        try {
          const data = JSON.parse(line.slice(6));
          onEvent({ type: data.type || 'text', data });
        } catch {
          // Skip malformed JSON
        }
      }
    }
  }
}

// ============================================================================
// Sessions
// ============================================================================

export interface SessionMeta {
  id: string;
  name: string;
  working_dir: string;
  project_hash: string;
  created_at: number;
  updated_at: number;
  message_count: number;
}

export async function listSessions(baseUrl: string): Promise<SessionMeta[]> {
  const resp = await fetch(`${baseUrl}/sessions`);
  if (!resp.ok) throw new Error(`Failed to list sessions: ${resp.statusText}`);
  return resp.json();
}

export async function createSession(baseUrl: string, workingDir?: string): Promise<SessionMeta> {
  const resp = await fetch(`${baseUrl}/sessions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ working_dir: workingDir }),
  });
  if (!resp.ok) throw new Error(`Failed to create session: ${resp.statusText}`);
  return resp.json();
}

// ============================================================================
// Models & Providers
// ============================================================================

export interface ModelInfo {
  provider: string;
  model: string;
  provider_type: string;
  is_default: boolean;
}

export async function getModels(baseUrl: string): Promise<ModelInfo[]> {
  const resp = await fetch(`${baseUrl}/models`);
  if (!resp.ok) throw new Error(`Failed to get models: ${resp.statusText}`);
  return resp.json();
}

// ============================================================================
// Skills
// ============================================================================

export interface SkillInfo {
  name: string;
  description: string;
  prompt: string;
}

export async function getSkills(baseUrl: string): Promise<SkillInfo[]> {
  const resp = await fetch(`${baseUrl}/skills`);
  if (!resp.ok) throw new Error(`Failed to get skills: ${resp.statusText}`);
  return resp.json();
}


