/* ------------------------------------------------------------------
   State types for the AtomCode Chat Webview
   ------------------------------------------------------------------ */

/** Model info returned by the daemon */
export interface ModelInfo {
  provider: string;
  model: string;
  provider_type?: string;
  is_default: boolean;
  effort_applicable?: boolean;
  reasoning_effort?: string | null;
}

export interface UserInfo {
  id: string;
  username: string;
  name?: string;
  email?: string;
  avatar_url?: string;
}

export interface AuthStatus {
  logged_in: boolean;
  expired?: boolean;
  auth_path: string;
  user: UserInfo | null;
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
  skip_tls_verify: boolean;
}

/** Lightweight session metadata (for the history list) */
export interface SessionMeta {
  id: string;
  name?: string;
  title?: string;
  created_at?: string | number;
  updated_at?: string | number;
  project_hash?: string;
  working_dir?: string;
  message_count?: number;
  file_size?: number;
  isGenerating?: boolean;
  hasUnread?: boolean;
}

/** A file or selection attached as context */
export interface ContextFile {
  path: string;
  fileName: string;
  language?: string;
  selection?: string;
  startLine?: number;
  endLine?: number;
  type: 'file' | 'selection';
}

export interface ImageData {
  media_type: string;
  data: string;
  missing?: boolean;
}

export interface SkillInfo {
  name: string;
  description: string;
}

export type ApprovalMode = 'build' | 'plan' | 'bypass' | 'accept_edits';
export type PermissionDecision = 'allow' | 'deny' | 'always_allow' | 'allow_persist';

/** Tool call data (collapsed section in the UI) */
export interface ToolCallData {
  id: string;
  name: string;
  args: string;
  output?: string;
  success?: boolean;
  durationMs?: number;
  progress?: string;
  status: 'queued' | 'running' | 'waiting_approval' | 'done' | 'error' | 'incomplete';
}

export type TodoStatus = 'pending' | 'in_progress' | 'completed';

export interface TodoItemData {
  content: string;
  status: TodoStatus;
}

export interface ArtifactData {
  id: string;
  artifactType: string;
  title?: string;
  language?: string;
  content: string;
  status: 'streaming' | 'complete';
}

export interface PermissionRequestData {
  id: string;
  sessionId: string;
  toolName: string;
  reason: string;
  args: string;
  isDestructive: boolean;
  status: 'pending' | 'submitting' | 'allowed' | 'denied';
  decision?: PermissionDecision;
  error?: string;
}

export interface StatusData {
  kind: 'warning' | 'rate_limited' | 'idle';
  message: string;
  retryAfterSeconds?: number;
  attempt?: number;
  maxAttempts?: number;
}

export type MessageBlock =
  | { id: string; type: 'text'; content: string }
  | { id: string; type: 'tool'; tool: ToolCallData }
  | { id: string; type: 'artifact'; artifact: ArtifactData }
  | { id: string; type: 'permission'; request: PermissionRequestData }
  | { id: string; type: 'status'; status: StatusData };

/** A single chat message (user or assistant) */
export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'error';
  text: string;
  blocks?: MessageBlock[];
  queued?: boolean;
  toolCalls?: ToolCallData[];
  artifacts?: ArtifactData[];
  permissionRequest?: PermissionRequestData;
  contextFiles?: ContextFile[];
  images?: ImageData[];
  streaming?: boolean;
  timestamp: number;
}

/** A single keyword match inside a text body. */
export interface SearchMatchRange {
  /** Start offset (inclusive). */
  start: number;
  /** Length of the match in characters. */
  length: number;
}

/** A logical search hit: a message id + the list of keyword ranges inside it. */
export interface SearchMatch {
  messageId: string;
  ranges: SearchMatchRange[];
}

/** Flat list of keyword matches, computed from the current query. */
export interface SearchState {
  /** All matches in display order; one entry per message that contains hits. */
  matches: SearchMatch[];
  /** Index into `matches` for the currently focused match (-1 when empty). */
  currentMatchIndex: number;
}

/** Root chat state */
export interface ChatState {
  messages: ChatMessage[];
  queuedMessages: ChatMessage[];
  activeTodos: TodoItemData[];
  isGenerating: boolean;
  recoveryLocked: boolean;
  isSessionList: boolean;
  viewMode: 'sidebar' | 'tab';
  currentModel: string;
  currentProvider: string;
  models: ModelInfo[];
  providers: ProviderInfo[];
  auth?: AuthStatus;
  setupRequired: boolean;
  setupStatus?: string;
  setupError?: string;
  loginUrl?: string;
  sessions: SessionMeta[];
  activeSessionId?: string;
  activeProjectHash?: string;
  contextFiles: ContextFile[];
  tokenCount?: { prompt: number; completion: number; total: number };
  historyOpen: boolean;
  settingsOpen: boolean;
  searchQuery: string;
  searchOpen: boolean;
  /** Derived from `messages` + `searchQuery` so the UI can navigate hits. */
  search: SearchState;
  /** Fullscreen image preview (lightbox) state; `null` when closed. */
  imagePreview: { images: ImageData[]; index: number } | null;
  locale?: string;
  approvalMode: ApprovalMode;
  approvalModePending: boolean;
  persistenceWarning?: string;
}

export type SessionTerminalState =
  | { type: 'done'; tokens?: number; toolCalls?: number; sessionId?: string; stopReason?: string; message?: string }
  | { type: 'stopped' }
  | { type: 'error'; message: string };

// ─── Actions dispatched by the reducer ──────────────────────────

export type ChatAction =
  | { type: 'ADD_USER_MESSAGE'; text: string; contextFiles?: ContextFile[]; images?: ImageData[] }
  | { type: 'ADD_QUEUED_MESSAGE'; id: string; text: string; contextFiles?: ContextFile[]; images?: ImageData[] }
  | { type: 'SEND_QUEUED_MESSAGE'; id: string }
  | { type: 'CLEAR_QUEUED_MESSAGES' }
  | { type: 'ADD_ASSISTANT_MESSAGE'; text: string }
  | { type: 'START_GENERATION' }
  | { type: 'APPEND_TEXT'; content: string }
  | { type: 'TOOL_BATCH_START'; calls: Array<{ id: string; name: string; args: string }> }
  | { type: 'TOOL_START'; id: string; name: string; args: string }
  | { type: 'TOOL_PROGRESS'; id: string; progress: string }
  | { type: 'TOOL_RESULT'; id: string; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'STREAM_WARNING'; message: string }
  | { type: 'SET_PERSISTENCE_WARNING'; message?: string }
  | { type: 'STREAM_RATE_LIMITED'; message: string; retryAfterSeconds?: number; attempt?: number; maxAttempts?: number }
  | { type: 'STREAM_IDLE_NOTICE'; message: string }
  | { type: 'ARTIFACT_START'; id: string; artifactType: string; language?: string; title?: string }
  | { type: 'ARTIFACT_CONTENT'; id: string; content: string }
  | { type: 'ARTIFACT_END'; id: string }
  | { type: 'SET_TOKENS'; prompt: number; completion: number; total: number }
  | { type: 'GENERATION_DONE'; tokens?: number }
  | { type: 'LOAD_SESSION_MESSAGES'; messages: Array<{ role: string; content: unknown; synthetic?: boolean; internal_origin?: string; internalOrigin?: string; images?: ImageData[]; tool_calls?: Array<{ id?: string; name?: string; arguments?: string; display?: string }>; tool_result?: { call_id?: string; success: boolean; summary: string; line_count: number }; artifacts?: Array<{ id: string; artifact_type?: string; artifactType?: string; title?: string; language?: string; content: string }> }>; terminal?: SessionTerminalState }
  | { type: 'GENERATION_STOPPED' }
  | { type: 'GENERATION_ERROR'; message: string }
  | { type: 'RECOVERY_REQUIRED' }
  | { type: 'RECOVERY_CLEARED' }
  | { type: 'CLEAR_CHAT' }
  | { type: 'SET_MODELS'; models: ModelInfo[] }
  | { type: 'SET_PROVIDERS'; providers: ProviderInfo[]; defaultProvider?: string }
  | { type: 'SET_AUTH'; auth: AuthStatus }
  | { type: 'SET_SETUP_STATE'; auth?: AuthStatus; providers: ProviderInfo[]; defaultProvider?: string; currentModel?: string; setupRequired: boolean }
  | { type: 'SET_SETUP_STATUS'; status?: string; error?: string; loginUrl?: string }
  | { type: 'SET_CURRENT_MODEL'; model: string }
  | { type: 'SET_CURRENT_PROVIDER'; provider: string; model?: string }
  | { type: 'SET_REASONING_EFFORT'; provider: string; effort: string | null }
  | { type: 'SET_APPROVAL_MODE'; mode: ApprovalMode; pending?: boolean }
  | { type: 'SET_SESSIONS'; sessions: SessionMeta[] }
  | { type: 'SET_ACTIVE_SESSION'; sessionId?: string; projectHash?: string }
  | { type: 'ADD_CONTEXT_FILE'; file: ContextFile }
  | { type: 'REMOVE_CONTEXT_FILE'; path: string; startLine?: number }
  | { type: 'CLEAR_CONTEXT' }
  | { type: 'TOGGLE_HISTORY' }
  | { type: 'TOGGLE_SETTINGS' }
  | { type: 'OPEN_IMAGE_PREVIEW'; images: ImageData[]; index: number }
  | { type: 'CLOSE_IMAGE_PREVIEW' }
  | { type: 'IMAGE_PREVIEW_NEXT' }
  | { type: 'IMAGE_PREVIEW_PREV' }
  | { type: 'PERMISSION_REQUEST'; id: string; sessionId: string; toolName: string; reason: string; args: string; isDestructive: boolean }
  | { type: 'PERMISSION_RESPOND'; id: string; decision: PermissionDecision }
  | { type: 'PERMISSION_RESPONSE_RESULT'; id: string; success: boolean; message?: string }
  | { type: 'SET_SEARCH_QUERY'; query: string }
  | { type: 'TOGGLE_SEARCH' }
  | { type: 'SEARCH_NEXT' }
  | { type: 'SEARCH_PREV' }
  | { type: 'RESUME_STREAMING' }
  | { type: 'INIT'; generating: boolean; recoveryLocked?: boolean; currentModel?: string; viewMode?: 'sidebar' | 'tab'; activeSessionId?: string; projectHash?: string; isSessionList?: boolean; locale?: string; approvalMode?: ApprovalMode; approvalModePending?: boolean };

// ─── Messages from the VS Code extension host ──────────────────

export type ExtensionMessage =
  | { type: 'init'; generating: boolean; recoveryLocked?: boolean; currentModel?: string; viewMode?: 'sidebar' | 'tab'; activeSessionId?: string; projectHash?: string; isSessionList?: boolean; locale?: string; approvalMode?: ApprovalMode; approvalModePending?: boolean }
  | { type: 'userMessage'; text: string; images?: ImageData[] }
  | { type: 'queuedMessageSent'; id: string }
  | { type: 'clearQueuedMessages' }
  | { type: 'assistantMessage'; text: string }
  | { type: 'generationStarted' }
  | { type: 'text'; content: string }
  | { type: 'toolBatchStart'; calls: Array<{ id: string; name: string; args: string }> }
  | { type: 'toolStart'; id?: string; name: string; args: string }
  | { type: 'toolProgress'; id: string; progress: string }
  | { type: 'toolResult'; id?: string; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'warning'; message: string }
  | { type: 'persistenceWarning'; message: string }
  | { type: 'rateLimited'; message: string; retryAfterSeconds?: number; attempt?: number; maxAttempts?: number }
  | { type: 'artifactStart'; id: string; artifactType: string; language?: string; title?: string }
  | { type: 'artifactContent'; id: string; content: string }
  | { type: 'artifactEnd'; id: string }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'done'; tokens?: number; toolCalls?: number; sessionId?: string; stopReason?: string; message?: string }
  | { type: 'sessionMessages'; messages: Array<{ role: string; content: unknown; synthetic?: boolean; internal_origin?: string; internalOrigin?: string; images?: ImageData[]; tool_calls?: Array<{ id?: string; name?: string; arguments?: string; display?: string }>; tool_result?: { call_id?: string; success: boolean; summary: string; line_count: number }; artifacts?: Array<{ id: string; artifact_type?: string; artifactType?: string; title?: string; language?: string; content: string }> }>; terminal?: SessionTerminalState }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
  | { type: 'recoveryRequired' }
  | { type: 'recoveryCleared' }
  | { type: 'generationStopped' }
  | { type: 'clearChat' }
  | { type: 'focusInput' }
  | { type: 'sessions'; sessions: SessionMeta[] }
  | { type: 'sessionSelected'; sessionId?: string; projectHash?: string }
  | { type: 'models'; models: ModelInfo[] }
  | { type: 'approvalMode'; mode: ApprovalMode; pending?: boolean }
  | { type: 'providers'; providers: ProviderInfo[]; defaultProvider?: string }
  | { type: 'authStatus'; auth: AuthStatus }
  | { type: 'setupState'; auth?: AuthStatus; providers: ProviderInfo[]; defaultProvider?: string; currentModel?: string; setupRequired: boolean }
  | { type: 'loginStarted'; loginId: string; url: string }
  | { type: 'loginPending' }
  | { type: 'loginAuthorized'; user: UserInfo | null }
  | { type: 'setupWorking'; message: string }
  | { type: 'codingPlanResult'; result: { success: boolean; report_text: string } }
  | { type: 'setupError'; message: string }
  | { type: 'context'; filePath: string; fileName: string; selection?: string; language?: string; startLine?: number; endLine?: number }
  | { type: 'insertText'; text: string }
  | { type: 'skills'; skills: SkillInfo[] }
  | { type: 'permissionRequest'; sessionId: string; id: string; toolName: string; reason: string; args: string; isDestructive: boolean }
  | { type: 'permissionResponseResult'; id: string; success: boolean; message?: string }
  | { type: 'resumeStreaming' }
  | { type: 'setDraft'; text: string }
  | { type: 'chromeFont'; value: string | null };
