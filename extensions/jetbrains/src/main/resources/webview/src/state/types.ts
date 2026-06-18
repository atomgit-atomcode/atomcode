export interface ChatViewModel {
  messages: MessageViewModel[];
  is_generating: boolean;
  is_waiting_permission: boolean;
  tokens: TokenUsageViewModel | null;
  pending_permission: PermissionViewModel | null;
  session_id: string | null;
  generation_error: string | null;
}

export interface MessageViewModel {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  reasoning?: string | null;
  status?: 'streaming' | 'complete' | null;
  tool_calls: ToolCallViewModel[];
  artifacts: ArtifactViewModel[];
  images: ImageViewModel[];
  context_files: string[];
  queued: boolean;
}

export interface ToolCallViewModel {
  call_id: string;
  name: string;
  arguments: string;
  output: string;
  success: boolean;
  duration_ms: number;
  status: 'queued' | 'running' | 'success' | 'error';
}

export interface ArtifactViewModel {
  id: string;
  artifact_type: string;
  title: string | null;
  language: string | null;
  content: string;
  status: 'started' | 'streaming' | 'complete';
}

export interface TokenUsageViewModel {
  prompt: number;
  completion: number;
  total: number;
}

export interface PermissionViewModel {
  tool_name: string;
  reason: string;
  call_id: string;
  arguments: string;
  severity: 'safe' | 'destructive' | 'critical';
  allow_persist: boolean;
}

export interface ImageViewModel {
  media_type: string;
  data: string;
}

export type UiCallback =
  | { type: 'ready' }
  | { type: 'copy_code'; code: string }
  | { type: 'open_file'; path: string; line?: number }
  | { type: 'permission_decision'; call_id: string; decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist' }
  | { type: 'scroll_complete' };

export interface UiState {
  model: ChatViewModel | null;
  isAutoScrolling: boolean;
}

export type UiAction =
  | { type: 'FULL_SYNC'; model: ChatViewModel }
  | { type: 'SET_AUTO_SCROLL'; value: boolean };
