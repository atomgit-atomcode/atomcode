import { ChatState, ChatAction, ChatMessage, ToolCallData, ContextFile } from './types';

let _msgCounter = 0;
function nextId(): string {
  return `msg-${Date.now()}-${++_msgCounter}`;
}

function lastAssistantIndex(messages: ChatMessage[]): number {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (messages[i].role === 'assistant') return i;
  }
  return -1;
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

// Matches the prefix emitted in provider.ts _handleSend when context files are attached.
const ATTACHED_FILES_PREFIX = 'The user has attached the following file(s) for context.';

function parseAttachedMessage(rawText: string): { displayText: string; contextFiles: ContextFile[] } {
  if (!rawText.startsWith(ATTACHED_FILES_PREFIX)) {
    return { displayText: rawText, contextFiles: [] };
  }

  const questionMarker = '\n\nUser question: ';
  const questionIdx = rawText.lastIndexOf(questionMarker);
  const userQuestion = questionIdx >= 0 ? rawText.slice(questionIdx + questionMarker.length).trim() : rawText;

  // Extract file names from ```<ext> fenced blocks.
  const contextFiles: ContextFile[] = [];
  const filePattern = /^File: (\S+)$/gm;
  let match: RegExpExecArray | null;
  while ((match = filePattern.exec(rawText)) !== null) {
    const fileName = match[1];
    if (!contextFiles.some((f) => f.fileName === fileName)) {
      contextFiles.push({
        path: fileName,
        fileName,
        type: 'file',
      });
    }
  }

  return { displayText: userQuestion, contextFiles };
}

export const initialState: ChatState = {
  messages: [],
  queuedMessages: [],
  isGenerating: false,
  isSessionList: document.body.dataset.viewMode === 'sidebar',
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

    case 'ADD_QUEUED_MESSAGE': {
      const msg: ChatMessage = {
        id: action.id,
        role: 'user',
        text: action.text,
        queued: true,
        contextFiles: action.contextFiles,
        timestamp: Date.now(),
      };
      return { ...state, queuedMessages: [...state.queuedMessages, msg] };
    }

    case 'SEND_QUEUED_MESSAGE': {
      const queued = state.queuedMessages.find((msg) => msg.id === action.id);
      if (!queued) return state;
      return {
        ...state,
        messages: [...state.messages, { ...queued, queued: false }],
        queuedMessages: state.queuedMessages.filter((msg) => msg.id !== action.id),
      };
    }

    case 'CLEAR_QUEUED_MESSAGES':
      return {
        ...state,
        queuedMessages: [],
      };

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

    // Resume a session that has an active background stream. Same as
    // START_GENERATION: create a fresh streaming assistant message that
    // subsequent text/toolStart events will append to.
    case 'RESUME_STREAMING': {
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
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = { ...assistant, text: assistant.text + action.content };
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_BATCH_START': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        const tools: ToolCallData[] = action.calls.map((c) => ({
          id: c.id,
          name: c.name,
          args: c.args,
          status: 'queued' as const,
        }));
        msgs[assistantIndex] = {
          ...assistant,
          toolCalls: [...(assistant.toolCalls ?? []), ...tools],
        };
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_START': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        const existingIndex = assistant.toolCalls?.findIndex((t) => t.id === action.id);
        if (existingIndex !== undefined && existingIndex >= 0) {
          // Tool was already announced via TOOL_BATCH_START — transition to running
          const updated = assistant.toolCalls!.map((t, i) =>
            i === existingIndex ? { ...t, args: action.args, status: 'running' as const } : t,
          );
          msgs[assistantIndex] = { ...assistant, toolCalls: updated };
        } else {
          // Legacy path: tool wasn't in a batch, add it directly as running
          const tool: ToolCallData = {
            id: action.id,
            name: action.name,
            args: action.args,
            status: 'running',
          };
          msgs[assistantIndex] = {
            ...assistant,
            toolCalls: [...(assistant.toolCalls ?? []), tool],
          };
        }
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_RESULT': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant?.toolCalls) {
        const tools = assistant.toolCalls.map((t) =>
          t.id === action.id
            ? { ...t, output: action.output, success: action.success, durationMs: action.durationMs, status: 'done' as const }
            : t,
        );
        msgs[assistantIndex] = { ...assistant, toolCalls: tools };
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
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = { ...assistant, streaming: false };
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
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = { ...assistant, streaming: false };
      }
      return { ...state, isGenerating: false, messages: msgs, queuedMessages: [] };
    }

    case 'GENERATION_ERROR': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = { ...assistant, streaming: false };
      }
      const errMsg: ChatMessage = {
        id: nextId(),
        role: 'error',
        text: action.message,
        timestamp: Date.now(),
      };
      return { ...state, isGenerating: false, messages: [...msgs, errMsg], queuedMessages: [] };
    }

    // ─── Session management ─────────────────────────
    case 'CLEAR_CHAT':
      return { ...state, messages: [], queuedMessages: [], tokenCount: undefined, contextFiles: [], isGenerating: false };

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
          // Find the last assistant message with tool calls — we need to update
          // it immutably (no direct mutation of objects already in the array).
          const lastAssistantIdx = messages.findLastIndex(
            (msg) => msg.role === 'assistant' && (msg.toolCalls?.length ?? 0) > 0,
          );
          if (lastAssistantIdx < 0) continue;

          const lastAssistant = messages[lastAssistantIdx];
          const callId = m.tool_result?.call_id;
          const output = textFromContent(m.content);

          if (lastAssistant.toolCalls && output) {
            const targetIndex = callId
              ? lastAssistant.toolCalls.findIndex((tool) => tool.id === callId)
              : lastAssistant.toolCalls.findIndex((tool) => tool.output === undefined);

            if (targetIndex >= 0) {
              // Immutable update: create new toolCalls array and new message object
              const newToolCalls = lastAssistant.toolCalls.map((tool, i) =>
                i === targetIndex
                  ? {
                      ...tool,
                      output,
                      success: m.tool_result?.success ?? true,
                      status: (m.tool_result?.success === false ? 'error' : 'done') as 'done' | 'error',
                    }
                  : tool,
              );
              messages[lastAssistantIdx] = { ...lastAssistant, toolCalls: newToolCalls };
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

        const rawText = textFromContent(m.content);

        // User messages may contain inline file content from the send path.
        // Parse it out into contextFiles so the UI shows attachment pills
        // instead of dumping the file body into the message bubble.
        const { displayText, contextFiles } = role === 'user'
          ? parseAttachedMessage(rawText)
          : { displayText: rawText, contextFiles: [] as ContextFile[] };

        messages.push({
          id: nextId(),
          role,
          text: displayText,
          toolCalls,
          contextFiles: contextFiles.length > 0 ? contextFiles : undefined,
          streaming: false,
          timestamp: Date.now(),
        });
      }
      return { ...state, messages, isGenerating: false };
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
        activeProjectHash: action.projectHash ?? state.activeProjectHash,
        isSessionList: action.isSessionList ?? state.isSessionList,
      };

    default:
      return state;
  }
}
