import { ChatState, ChatAction, ChatMessage, ToolCallData } from './types';

let _msgCounter = 0;
function nextId(): string {
  return `msg-${Date.now()}-${++_msgCounter}`;
}

export const initialState: ChatState = {
  messages: [],
  isGenerating: false,
  currentModel: 'default',
  models: [],
  sessions: [],
  contextFiles: [],
  tokenCount: undefined,
  historyOpen: false,
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

    case 'SET_CURRENT_MODEL':
      return { ...state, currentModel: action.model };

    case 'SET_SESSIONS':
      return { ...state, sessions: action.sessions };

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

    case 'TOGGLE_HISTORY':
      return { ...state, historyOpen: !state.historyOpen };

    case 'LOAD_SESSION_MESSAGES': {
      // Convert daemon message format to our ChatMessage format
      const messages: ChatMessage[] = action.messages
        .filter((m: { role: string }) => m.role === 'user' || m.role === 'assistant')
        .map((m: { role: string; content: string }) => ({
          id: nextId(),
          role: m.role as 'user' | 'assistant',
          text: m.content || '',
          toolCalls: [],
          streaming: false,
          timestamp: Date.now(),
        }));
      return { ...state, messages };
    }

    // ─── Init ───────────────────────────────────────
    case 'INIT':
      return {
        ...state,
        isGenerating: action.generating,
        currentModel: action.currentModel ?? state.currentModel,
      };

    default:
      return state;
  }
}
