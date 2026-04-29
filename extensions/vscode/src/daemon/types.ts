// Chat
export interface ChatRequest {
  message: string;
  working_dir?: string;
  provider?: string;
  session_id?: string;
}

export type ChatEvent =
  | { type: 'text'; content: string }
  | { type: 'tool_start'; name: string; arguments: string }
  | { type: 'tool_result'; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'artifact_start'; id: string; artifact_type: string; language?: string; title?: string }
  | { type: 'artifact_content'; id: string; content: string }
  | { type: 'artifact_end'; id: string }
  | { type: 'done'; tokens: number; tool_calls: number }
  | { type: 'stopped' }
  | { type: 'error'; message: string };

// Health
export interface HealthResponse {
  status: string;
  version: string;
  service: string;
}

// Project
export interface ProjectState {
  working_dir: string;
  previous_dir?: string;
  recent_dirs: string[];
  name: string;
}

export interface ChangeDirResponse {
  success: boolean;
  message: string;
  current_dir: string;
  project_hash: string;
}

// Models
export interface ModelInfo {
  provider: string;
  model: string;
  provider_type: string;
  is_default: boolean;
}

// Sessions
export interface SessionMeta {
  id: string;
  name: string;
  created_at: number;
  updated_at: number;
  message_count: number;
  file_size: number;
}

export interface SessionDetail {
  id: string;
  name: string;
  working_dir: string;
  created_at: number;
  updated_at: number;
  message_count: number;
  messages: MessageInfo[];
}

export interface MessageInfo {
  role: string;
  content: string;
  tool_calls?: ToolCallInfo[];
  tool_result?: ToolResultInfo;
  artifacts?: ArtifactInfo[];
}

export interface ToolCallInfo {
  id: string;
  name: string;
  arguments: string;
  display: string;
}

export interface ToolResultInfo {
  success: boolean;
  summary: string;
  line_count: number;
}

export interface ArtifactInfo {
  id: string;
  artifact_type: string;
  title?: string;
  language?: string;
  content: string;
}

export interface CreateSessionResponse {
  id: string;
  name: string;
  working_dir: string;
  project_hash: string;
  created_at: number;
}

// Callbacks for SSE streaming
export interface ChatStreamCallbacks {
  onText: (content: string) => void;
  onToolStart: (name: string, args: string) => void;
  onToolResult: (name: string, output: string, success: boolean, durationMs: number) => void;
  onTokens: (prompt: number, completion: number, total: number) => void;
  onArtifactStart: (id: string, type: string, language?: string, title?: string) => void;
  onArtifactContent: (id: string, content: string) => void;
  onArtifactEnd: (id: string) => void;
  onDone: (tokens: number, toolCalls: number) => void;
  onStopped: () => void;
  onError: (message: string) => void;
}
