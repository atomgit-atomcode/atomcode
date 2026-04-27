/* ------------------------------------------------------------------
   State types for the AtomCode Chat Webview
   ------------------------------------------------------------------ */

/** Model info returned by the daemon */
export interface ModelInfo {
  provider: string;
  model: string;
  is_default: boolean;
}

/** Lightweight session metadata (for the history list) */
export interface SessionMeta {
  id: string;
  name?: string;
  title?: string;
  created_at?: string;
  updated_at?: string;
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
  status: 'running' | 'done' | 'error';
}

/** A single chat message (user or assistant) */
export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'error';
  text: string;
  toolCalls?: ToolCallData[];
  contextFiles?: ContextFile[];
  streaming?: boolean;
  timestamp: number;
}

/** Root chat state */
export interface ChatState {
  messages: ChatMessage[];
  isGenerating: boolean;
  currentModel: string;
  models: ModelInfo[];
  sessions: SessionMeta[];
  contextFiles: ContextFile[];
  tokenCount?: { prompt: number; completion: number; total: number };
}

// ─── Actions dispatched by the reducer ──────────────────────────

export type ChatAction =
  | { type: 'ADD_USER_MESSAGE'; text: string; contextFiles?: ContextFile[] }
  | { type: 'START_GENERATION' }
  | { type: 'APPEND_TEXT'; content: string }
  | { type: 'TOOL_START'; id: string; name: string; args: string }
  | { type: 'TOOL_RESULT'; id: string; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'SET_TOKENS'; prompt: number; completion: number; total: number }
  | { type: 'GENERATION_DONE'; tokens?: { prompt: number; completion: number; total: number } }
  | { type: 'GENERATION_STOPPED' }
  | { type: 'GENERATION_ERROR'; message: string }
  | { type: 'CLEAR_CHAT' }
  | { type: 'SET_MODELS'; models: ModelInfo[] }
  | { type: 'SET_CURRENT_MODEL'; model: string }
  | { type: 'SET_SESSIONS'; sessions: SessionMeta[] }
  | { type: 'ADD_CONTEXT_FILE'; file: ContextFile }
  | { type: 'REMOVE_CONTEXT_FILE'; path: string }
  | { type: 'INIT'; generating: boolean; currentModel?: string };

// ─── Messages from the VS Code extension host ──────────────────

export type ExtensionMessage =
  | { type: 'init'; generating: boolean; currentModel?: string }
  | { type: 'userMessage'; text: string }
  | { type: 'generationStarted' }
  | { type: 'text'; content: string }
  | { type: 'toolStart'; name: string; args: string }
  | { type: 'toolResult'; name: string; output: string; success: boolean; durationMs: number }
  | { type: 'tokens'; prompt: number; completion: number; total: number }
  | { type: 'done'; tokens?: { prompt: number; completion: number; total: number }; toolCalls?: number }
  | { type: 'stopped' }
  | { type: 'error'; message: string }
  | { type: 'generationStopped' }
  | { type: 'clearChat' }
  | { type: 'focusInput' }
  | { type: 'sessions'; sessions: SessionMeta[] }
  | { type: 'models'; models: ModelInfo[] }
  | { type: 'context'; filePath: string; fileName: string; selection?: string; language?: string };
