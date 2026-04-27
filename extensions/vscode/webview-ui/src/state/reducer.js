"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.initialState = void 0;
exports.chatReducer = chatReducer;
let _msgCounter = 0;
function nextId() {
    return `msg-${Date.now()}-${++_msgCounter}`;
}
exports.initialState = {
    messages: [],
    isGenerating: false,
    currentModel: 'default',
    models: [],
    sessions: [],
    contextFiles: [],
    tokenCount: undefined,
};
function chatReducer(state, action) {
    switch (action.type) {
        // ─── User sends a message ────────────────────────
        case 'ADD_USER_MESSAGE': {
            const msg = {
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
            const assistant = {
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
                const tool = {
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
                const tools = last.toolCalls.map((t) => t.id === action.id
                    ? { ...t, output: action.output, success: action.success, durationMs: action.durationMs, status: 'done' }
                    : t);
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
            return {
                ...state,
                isGenerating: false,
                messages: msgs,
                tokenCount: action.tokens ?? state.tokenCount,
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
            const errMsg = {
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
            if (state.contextFiles.some((f) => f.path === action.file.path))
                return state;
            return { ...state, contextFiles: [...state.contextFiles, action.file] };
        }
        case 'REMOVE_CONTEXT_FILE':
            return {
                ...state,
                contextFiles: state.contextFiles.filter((f) => f.path !== action.path),
            };
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
//# sourceMappingURL=reducer.js.map