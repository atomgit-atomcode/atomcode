import { ChatState, ChatAction, ChatMessage, ToolCallData } from './types';

let _msgCounter = 0;
function nextId(): string {
  return `msg-${Date.now()}-${++_msgCounter}`;
}

function normalizeRole(role: string): 'user' | 'assistant' | 'tool' | 'system' | 'unknown' {
  const normalized = String(role || '').toLowerCase();
  if (normalized === 'user' || normalized === 'assistant' || normalized === 'tool' || normalized === 'system') {
    return normalized;
  }
  return 'unknown';
}

function textFromContent(content: unknown): string {
  if (typeof content === 'string') return content;
  if (!content || typeof content !== 'object') return '';

  const value = content as {
    Text?: unknown;
    AssistantWithToolCalls?: { text?: unknown };
    ToolResult?: { output?: unknown };
    ToolResultRef?: { summary?: unknown };
  };

  if (typeof value.Text === 'string') return value.Text;
  if (value.AssistantWithToolCalls) {
    return typeof value.AssistantWithToolCalls.text === 'string' ? value.AssistantWithToolCalls.text : '';
  }
  if (value.ToolResult) {
    return typeof value.ToolResult.output === 'string' ? value.ToolResult.output : '';
  }
  if (value.ToolResultRef) {
    return typeof value.ToolResultRef.summary === 'string' ? value.ToolResultRef.summary : '';
  }

  return '';
}

export const initialState: ChatState = {
  messages: [],
  isGenerating: false,
  viewMode: document.body.dataset.viewMode === 'sidebar' ? 'sidebar' : 'tab',
  currentModel: 'default',
  currentProvider: '',
  models: [],
  providers: [],
  auth: undefined,
  setupRequired: false,
  setupStatus: undefined,
  setupError: undefined,
  loginUrl: undefined,
  sessions: [],
  activeSessionId: undefined,
  activeProjectHash: undefined,
  contextFiles: [],
  tokenCount: undefined,
  historyOpen: false,
  settingsOpen: false,
  searchQuery: '',
  searchOpen: false,
};

export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    // ─── User sends a message ────────────────────────
    case 'ADD_USER_MESSAGE': {
      const msg: ChatMessage = {
        id: nextId(),
        role: 'user',
        text: action.text,
        contextFiles: action.contextFiles,
        timestamp: Date.now(),
      };
      return { ...state, messages: [...state.messages, msg] };
    }

    case 'ADD_ASSISTANT_MESSAGE': {
      const msg: ChatMessage = {
        id: nextId(),
        role: 'assistant',
        text: action.text,
        toolCalls: [],
        streaming: false,
        timestamp: Date.now(),
      };
      return { ...state, messages: [...state.messages, msg] };
    }

    // ─── Generation lifecycle ────────────────────────
    case 'START_GENERATION': {
      const assistant: ChatMessage = {
        id: nextId(),
        role: 'assistant',
        text: '',
        toolCalls: [],
        streaming: true,
        timestamp: Date.now(),
      };
      return {
        ...state,
        isGenerating: true,
        messages: [...state.messages, assistant],
      };
    }

    case 'APPEND_TEXT': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = { ...last, text: last.text + action.content };
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_START': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        const tool: ToolCallData = {
          id: action.id,
          name: action.name,
          args: action.args,
          status: 'running',
        };
        msgs[msgs.length - 1] = {
          ...last,
          toolCalls: [...(last.toolCalls ?? []), tool],
        };
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_RESULT': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant' && last.toolCalls) {
        const tools = last.toolCalls.map((t) =>
          t.id === action.id
            ? { ...t, output: action.output, success: action.success, durationMs: action.durationMs, status: 'done' as const }
            : t,
        );
        msgs[msgs.length - 1] = { ...last, toolCalls: tools };
      }
      return { ...state, messages: msgs };
    }

    case 'SET_TOKENS':
      return {
        ...state,
        tokenCount: { prompt: action.prompt, completion: action.completion, total: action.total },
      };

    case 'GENERATION_DONE': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = { ...last, streaming: false };
      }
      // action.tokens is a number (total), not a tokenCount object
      const tokenCount = typeof action.tokens === 'number'
        ? { prompt: 0, completion: 0, total: action.tokens }
        : state.tokenCount;
      return {
        ...state,
        isGenerating: false,
        messages: msgs,
        tokenCount,
      };
    }

    case 'GENERATION_STOPPED': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = { ...last, streaming: false };
      }
      return { ...state, isGenerating: false, messages: msgs };
    }

    case 'GENERATION_ERROR': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = { ...last, streaming: false };
      }
      const errMsg: ChatMessage = {
        id: nextId(),
        role: 'error',
        text: action.message,
        timestamp: Date.now(),
      };
      return { ...state, isGenerating: false, messages: [...msgs, errMsg] };
    }

    // ─── Session management ─────────────────────────
    case 'CLEAR_CHAT':
      return { ...state, messages: [], tokenCount: undefined, contextFiles: [] };

    case 'SET_MODELS':
      return { ...state, models: action.models };

    case 'SET_PROVIDERS': {
      const current = action.providers.find((p) => p.name === action.defaultProvider)
        ?? action.providers.find((p) => p.is_default);
      return {
        ...state,
        providers: action.providers,
        currentProvider: current?.name ?? state.currentProvider,
        currentModel: current?.model ?? state.currentModel,
        setupRequired: state.auth?.logged_in === false || action.providers.length === 0,
      };
    }

    case 'SET_AUTH':
      return {
        ...state,
        auth: action.auth,
        setupRequired: !action.auth.logged_in || state.providers.length === 0,
      };

    case 'SET_SETUP_STATE': {
      const current = action.providers.find((p) => p.name === action.defaultProvider)
        ?? action.providers.find((p) => p.is_default);
      return {
        ...state,
        auth: action.auth ?? state.auth,
        providers: action.providers,
        currentProvider: current?.name ?? action.defaultProvider ?? state.currentProvider,
        currentModel: action.currentModel ?? current?.model ?? state.currentModel,
        setupRequired: action.setupRequired,
        setupError: undefined,
        setupStatus: action.setupRequired ? state.setupStatus : undefined,
      };
    }

    case 'SET_SETUP_STATUS':
      return {
        ...state,
        setupStatus: action.status,
        setupError: action.error,
        loginUrl: action.loginUrl ?? state.loginUrl,
      };

    case 'SET_CURRENT_MODEL':
      return { ...state, currentModel: action.model };

    case 'SET_CURRENT_PROVIDER': {
      const provider = state.providers.find((p) => p.name === action.provider);
      return {
        ...state,
        currentProvider: action.provider,
        currentModel: action.model ?? provider?.model ?? state.currentModel,
      };
    }

    case 'SET_SESSIONS':
      return { ...state, sessions: action.sessions };

    case 'SET_ACTIVE_SESSION':
      return {
        ...state,
        activeSessionId: action.sessionId,
        activeProjectHash: action.projectHash,
      };

    // ─── Context files ──────────────────────────────
    case 'ADD_CONTEXT_FILE': {
      if (state.contextFiles.some((f) => f.path === action.file.path)) return state;
      return { ...state, contextFiles: [...state.contextFiles, action.file] };
    }

    case 'REMOVE_CONTEXT_FILE':
      return {
        ...state,
        contextFiles: state.contextFiles.filter((f) => f.path !== action.path),
      };

    case 'CLEAR_CONTEXT':
      return { ...state, contextFiles: [] };

    case 'TOGGLE_HISTORY':
      return { ...state, historyOpen: !state.historyOpen };

    case 'TOGGLE_SETTINGS':
      return { ...state, settingsOpen: !state.settingsOpen };

    case 'LOAD_SESSION_MESSAGES': {
      // Convert daemon message format to our ChatMessage format
      const messages: ChatMessage[] = [];

      for (const m of action.messages) {
        const role = normalizeRole(m.role);

        if (role === 'tool') {
          const lastAssistant = messages.findLast((msg) => msg.role === 'assistant' && msg.toolCalls?.length);
          const callId = m.tool_result?.call_id;
          const output = textFromContent(m.content);
          if (lastAssistant?.toolCalls && output) {
            const targetIndex = callId
              ? lastAssistant.toolCalls.findIndex((tool) => tool.id === callId)
              : lastAssistant.toolCalls.findIndex((tool) => tool.output === undefined);
            if (targetIndex >= 0) {
              lastAssistant.toolCalls[targetIndex] = {
                ...lastAssistant.toolCalls[targetIndex],
                output,
                success: m.tool_result?.success ?? true,
                status: m.tool_result?.success === false ? 'error' : 'done',
              };
            }
          }
          continue;
        }

        if (role !== 'user' && role !== 'assistant') {
          continue;
        }

        const toolCalls: ToolCallData[] = (m.tool_calls ?? []).map((tool, index) => ({
          id: tool.id || `history-tool-${index}`,
          name: tool.name || 'tool',
          args: tool.arguments || '',
          success: true,
          status: 'done',
        }));

        messages.push({
          id: nextId(),
          role,
          text: textFromContent(m.content),
          toolCalls,
          streaming: false,
          timestamp: Date.now(),
        });
      }
      return { ...state, messages };
    }

    case 'SET_SEARCH_QUERY':
      return { ...state, searchQuery: action.query };

    case 'TOGGLE_SEARCH':
      return { ...state, searchOpen: !state.searchOpen, searchQuery: state.searchOpen ? '' : state.searchQuery };

    case 'PERMISSION_REQUEST': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = {
          ...last,
          permissionRequest: {
            id: action.id,
            toolName: action.toolName,
            args: action.args,
            isDestructive: action.isDestructive,
            status: 'pending',
          },
        };
      }
      return { ...state, messages: msgs };
    }

    case 'PERMISSION_RESPOND': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant' && last.permissionRequest?.id === action.id) {
        msgs[msgs.length - 1] = {
          ...last,
          permissionRequest: {
            ...last.permissionRequest,
            status: action.allowed ? 'allowed' : 'denied',
          },
        };
      }
      return { ...state, messages: msgs };
    }

    // ─── Init ───────────────────────────────────────
    case 'INIT':
      return {
        ...state,
        isGenerating: action.generating,
        currentModel: action.currentModel ?? state.currentModel,
        viewMode: action.viewMode ?? state.viewMode,
        activeSessionId: action.activeSessionId ?? state.activeSessionId,
      };

    default:
      return state;
  }
}
