export type ApprovalMode = 'build' | 'plan' | 'bypass' | 'accept_edits';

// Chat
export interface ChatRequest {
  message: string;
  working_dir?: string;
  provider?: string;
  session_id?: string;
  images?: ImageInput[];
  approval_mode?: ApprovalMode;
}

export interface ImageInput {
  media_type: string;
  data: string;
  missing?: boolean;
}

export type ChatStopReason =
  | 'stopped'
  | 'max_rounds'
  | 'max_continuations'
  | 'repeat_loop'
  | 'tool_loop_detected'
  | 'provider_error'
  | 'timeout'
  | 'cancelled'
  | 'prompt_rejected'
  | 'rate_limited';

export interface SkillInfo {
  name: string;
  description: string;
}

export interface ApprovalModeResponse {
  ok: boolean;
  mode: ApprovalMode;
}

export type ChatEvent =
  | { type: 'runtime_info'; provider: string; model: string }
  | { type: 'mode'; mode: ApprovalMode }
  | { type: 'text'; content: string }
  | { type: 'tool_batch'; calls: Array<{ id: string; name: string; arguments: string }> }
  | { type: 'tool_start'; id?: string; name: string; arguments: string }
  | { type: 'tool_progress'; id: string; progress: string }
  | { type: 'tool_result'; id?: string; name: string; output: string; success: boolean; duration_ms: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'artifact_start'; id: string; artifact_type: string; language?: string; title?: string }
  | { type: 'artifact_content'; id: string; content: string }
  | { type: 'artifact_end'; id: string }
  | { type: 'permission_request'; session_id: string; tool_name: string; reason: string; call_id: string; arguments: string }
  | { type: 'warning'; message: string }
  | { type: 'rate_limited'; message: string; retry_after_seconds?: number; attempt?: number; max_attempts?: number }
  | { type: 'done'; tokens: number; tool_calls: number; session_id?: string; stop_reason?: ChatStopReason; message?: string }
  | { type: 'stopped' }
  | { type: 'error'; message: string };

export type PermissionDecision = 'allow' | 'deny' | 'always_allow' | 'allow_persist';

export interface PermissionDecisionResponse {
  success: boolean;
  error?: string;
}

// Health
export interface HealthResponse {
  status: string;
  version: string;
  service: string;
  instance_id?: string;
}

// Project
export interface ProjectState {
  working_dir: string;
  previous_dir?: string;
  recent_dirs: string[];
  name: string;
  project_hash: string;
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
  effort_applicable?: boolean;
  reasoning_effort?: string | null;
  has_api_key?: boolean;
  base_url?: string;
  thinking_enabled?: boolean;
  thinking_budget?: number;
}

// Auth / config / providers
export interface UserInfo {
  id: string;
  username: string;
  name?: string;
  email?: string;
  avatar_url?: string;
}

export interface AuthStatusResponse {
  logged_in: boolean;
  expired: boolean;
  auth_path: string;
  user: UserInfo | null;
  token: {
    token_type: string;
    expires_in?: number;
    created_at: number;
    has_refresh_token: boolean;
  } | null;
}

export interface LoginStartResponse {
  login_id: string;
  url: string;
  expires_in_seconds: number;
  daemon_instance_id?: string;
}

export interface LoginPollResponse {
  status: 'pending' | 'authorized' | 'expired' | 'cancelled' | 'failed';
  user: UserInfo | null;
  code?: string;
  message?: string;
  retry_after_ms?: number;
}

export interface ProviderInfo {
  name: string;
  type: string;
  model: string;
  base_url?: string;
  has_api_key: boolean;
  requires_login?: boolean;
  is_default: boolean;
  context_window: number;
  max_tokens?: number;
  thinking_enabled?: boolean;
  thinking_budget?: number;
  thinking_type?: string;
  thinking_keep?: string;
  reasoning_history?: string;
  reasoning_effort?: string | null;
  skip_tls_verify: boolean;
  ephemeral: boolean;
}

export interface ConfigResponse {
  path: string;
  default_provider: string;
  default_workdir?: string;
  providers: ProviderInfo[];
}

export interface ProvidersResponse {
  default_provider: string;
  providers: ProviderInfo[];
}

export interface CreateProviderRequest {
  name: string;
  type: string;
  model: string;
  api_key?: string;
  base_url?: string;
  user_agent?: string;
  context_window?: number;
  max_tokens?: number;
  thinking_type?: string;
  thinking_keep?: string;
  reasoning_history?: string;
  thinking_enabled?: boolean;
  thinking_budget?: number;
  skip_tls_verify?: boolean;
  set_default?: boolean;
}

export interface PatchProviderRequest {
  type?: string;
  model?: string;
  api_key?: string | null;
  clear_api_key?: boolean;
  base_url?: string | null;
  clear_base_url?: boolean;
  user_agent?: string | null;
  clear_user_agent?: boolean;
  context_window?: number;
  max_tokens?: number | null;
  clear_max_tokens?: boolean;
  thinking_enabled?: boolean | null;
  thinking_budget?: number | null;
  thinking_type?: string | null;
  thinking_keep?: string | null;
  reasoning_history?: string | null;
  skip_tls_verify?: boolean;
}

export interface PatchThinkingRequest {
  enabled?: boolean;
  budget?: number;
  type?: string | null;
  keep?: string | null;
  reasoning_history?: string | null;
}

export interface CodingPlanSetupResponse {
  success: boolean;
  report_text: string;
  default_provider: string;
  providers: ProviderInfo[];
  steps: {
    login: { status: string; message: string };
    claim: { status: string; message: string };
    models: { status: string; message: string };
    status: { status: string; message: string };
  };
}

// Sessions
export interface SessionMeta {
  id: string;
  name: string;
  working_dir?: string;
  project_hash?: string;
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
  synthetic?: boolean;
  internal_origin?: string;
  internalOrigin?: string;
  images?: ImageInput[];
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
  call_id?: string;
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

export interface AppendSessionMessage {
  role: 'user' | 'assistant';
  content: string;
}

export interface AppendSessionMessagesRequest {
  working_dir?: string;
  messages: AppendSessionMessage[];
}

export interface AppendSessionMessagesResponse {
  success: boolean;
  session_id: string;
  message_count: number;
  project_hash: string;
}

// Callbacks for SSE streaming
export interface ChatStreamCallbacks {
  onRuntimeInfo?: (provider: string, model: string) => void;
  onMode?: (mode: ApprovalMode) => void;
  onText: (content: string) => void;
  onToolBatch: (calls: Array<{ id: string; name: string; args: string }>) => void;
  onToolStart: (id: string | undefined, name: string, args: string) => void;
  onToolProgress: (id: string, progress: string) => void;
  onToolResult: (id: string | undefined, name: string, output: string, success: boolean, durationMs: number) => void;
  onPermissionRequest: (request: { sessionId: string; toolName: string; reason: string; callId: string; args: string }) => void;
  onTokens: (prompt: number, completion: number, total: number) => void;
  onArtifactStart: (id: string, type: string, language?: string, title?: string) => void;
  onArtifactContent: (id: string, content: string) => void;
  onArtifactEnd: (id: string) => void;
  onWarning: (message: string) => void;
  onRateLimited: (event: { message: string; retryAfterSeconds?: number; attempt?: number; maxAttempts?: number }) => void;
  onDone: (tokens: number, toolCalls: number, sessionId?: string, stopReason?: ChatStopReason, message?: string) => void;
  onStopped: () => void;
  onError: (message: string) => void;
}
