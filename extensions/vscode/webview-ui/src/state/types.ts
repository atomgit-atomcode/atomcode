/* ------------------------------------------------------------------
   State types for the AtomCode Chat Webview
   ------------------------------------------------------------------ */

/** Model info returned by the daemon */
export interface ModelInfo {
  provider: string;
  model: string;
  provider_type?: string;
  is_default: boolean;
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
  auth_path: string;
  user: UserInfo | null;
}

export interface ProviderInfo {
  name: string;
  type: string;
  model: string;
  base_url?: string;
  has_api_key: boolean;
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
  isGenerating?: boolean;
  hasUnread?: boolean;
}

/** A file or selection attached as context */
export interface ContextFile {
  path: string;
  fileName: string;
  language?: string;
  selection?: string;
  type: 'file' | 'selection';
}

/** Tool call data (collapsed section in the UI) */
export interface ToolCallData {
  id: string;
  name: string;
  args: string;
  output?: string;
  success?: boolean;
  durationMs?: number;
  status: 'queued' | 'running' | 'done' | 'error';
}

export interface PermissionRequestData {
  id: string;
  toolName: string;
  args: string;
  isDestructive: boolean;
  status: 'pending' | 'allowed' | 'denied';
}

/** A single chat message (user or assistant) */
export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'error';
  text: string;
  queued?: boolean;
  toolCalls?: ToolCallData[];
  permissionRequest?: PermissionRequestData;
  contextFiles?: ContextFile[];
  streaming?: boolean;
  timestamp: number;
}

/** Root chat state */
export interface ChatState {
  messages: ChatMessage[];
  queuedMessages: ChatMessage[];
  isGenerating: boolean;
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
}

// ─── Actions dispatched by the reducer ──────────────────────────

export type ChatAction =
  | { type: 'ADD_USER_MESSAGE'; text: string; contextFiles?: ContextFile[] }
  | { type: 'ADD_QUEUED_MESSAGE'; id: string; text: string; contextFiles?: ContextFile[] }
  | { type: 'SEND_QUEUED_MESSAGE'; id: string }
  | { type: 'CLEAR_QUEUED_MESSAGES' }
  | { type: 'ADD_ASSISTANT_MESSAGE'; text: string }
  | { type: 'START_GENERATION' }
  | { type: 'APPEND_TEXT'; content: string }
  | { type: 'TOOL_BATCH_START'; calls: Array<{ id: string; name: string; args: string }> }
  | { type: 'TOOL_START'; id: string; name: string; args: string }
  | { type: 'TOOL_RESULT'; id: string; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'SET_TOKENS'; prompt: number; completion: number; total: number }
  | { type: 'GENERATION_DONE'; tokens?: number }
  | { type: 'LOAD_SESSION_MESSAGES'; messages: Array<{ role: string; content: unknown; tool_calls?: Array<{ id?: string; name?: string; arguments?: string; display?: string }>; tool_result?: { call_id?: string; success: boolean; summary: string; line_count: number } }> }
  | { type: 'GENERATION_STOPPED' }
  | { type: 'GENERATION_ERROR'; message: string }
  | { type: 'CLEAR_CHAT' }
  | { type: 'SET_MODELS'; models: ModelInfo[] }
  | { type: 'SET_PROVIDERS'; providers: ProviderInfo[]; defaultProvider?: string }
  | { type: 'SET_AUTH'; auth: AuthStatus }
  | { type: 'SET_SETUP_STATE'; auth?: AuthStatus; providers: ProviderInfo[]; defaultProvider?: string; currentModel?: string; setupRequired: boolean }
  | { type: 'SET_SETUP_STATUS'; status?: string; error?: string; loginUrl?: string }
  | { type: 'SET_CURRENT_MODEL'; model: string }
  | { type: 'SET_CURRENT_PROVIDER'; provider: string; model?: string }
  | { type: 'SET_SESSIONS'; sessions: SessionMeta[] }
  | { type: 'SET_ACTIVE_SESSION'; sessionId?: string; projectHash?: string }
  | { type: 'ADD_CONTEXT_FILE'; file: ContextFile }
  | { type: 'REMOVE_CONTEXT_FILE'; path: string }
  | { type: 'CLEAR_CONTEXT' }
  | { type: 'TOGGLE_HISTORY' }
  | { type: 'TOGGLE_SETTINGS' }
  | { type: 'PERMISSION_REQUEST'; id: string; toolName: string; args: string; isDestructive: boolean }
  | { type: 'PERMISSION_RESPOND'; id: string; allowed: boolean }
  | { type: 'SET_SEARCH_QUERY'; query: string }
  | { type: 'TOGGLE_SEARCH' }
  | { type: 'RESUME_STREAMING' }
  | { type: 'INIT'; generating: boolean; currentModel?: string; viewMode?: 'sidebar' | 'tab'; activeSessionId?: string; projectHash?: string; isSessionList?: boolean };

// ─── Messages from the VS Code extension host ──────────────────

export type ExtensionMessage =
  | { type: 'init'; generating: boolean; currentModel?: string; viewMode?: 'sidebar' | 'tab'; activeSessionId?: string; projectHash?: string; isSessionList?: boolean }
  | { type: 'userMessage'; text: string }
  | { type: 'queuedMessageSent'; id: string }
  | { type: 'assistantMessage'; text: string }
  | { type: 'generationStarted' }
  | { type: 'text'; content: string }
  | { type: 'toolBatchStart'; calls: Array<{ id: string; name: string; args: string }> }
  | { type: 'toolStart'; id?: string; name: string; args: string }
  | { type: 'toolResult'; id?: string; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'done'; tokens?: number; toolCalls?: number; sessionId?: string }
  | { type: 'sessionMessages'; messages: Array<{ role: string; content: unknown; tool_calls?: Array<{ id?: string; name?: string; arguments?: string; display?: string }>; tool_result?: { call_id?: string; success: boolean; summary: string; line_count: number } }> }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
  | { type: 'generationStopped' }
  | { type: 'clearChat' }
  | { type: 'focusInput' }
  | { type: 'sessions'; sessions: SessionMeta[] }
  | { type: 'sessionSelected'; sessionId?: string; projectHash?: string }
  | { type: 'models'; models: ModelInfo[] }
  | { type: 'providers'; providers: ProviderInfo[]; defaultProvider?: string }
  | { type: 'authStatus'; auth: AuthStatus }
  | { type: 'setupState'; auth?: AuthStatus; providers: ProviderInfo[]; defaultProvider?: string; currentModel?: string; setupRequired: boolean }
  | { type: 'loginStarted'; loginId: string; url: string }
  | { type: 'loginPending' }
  | { type: 'loginAuthorized'; user: UserInfo | null }
  | { type: 'setupWorking'; message: string }
  | { type: 'codingPlanResult'; result: { success: boolean; report_text: string } }
  | { type: 'setupError'; message: string }
  | { type: 'context'; filePath: string; fileName: string; selection?: string; language?: string }
  | { type: 'permissionRequest'; id: string; toolName: string; args: string; isDestructive: boolean }
  | { type: 'resumeStreaming' }
  | { type: 'setDraft'; text: string };
