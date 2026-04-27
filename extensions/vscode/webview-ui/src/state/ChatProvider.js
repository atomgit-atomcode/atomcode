"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.useChatContext = useChatContext;
exports.ChatProvider = ChatProvider;
const react_1 = __importStar(require("react"));
const reducer_1 = require("./reducer");
const vscode_1 = require("../vscode");
const ChatContext = (0, react_1.createContext)(null);
function useChatContext() {
    const ctx = (0, react_1.useContext)(ChatContext);
    if (!ctx)
        throw new Error('useChatContext must be used inside <ChatProvider>');
    return ctx;
}
// ─── Provider ───────────────────────────────────────────────────
let _toolIdCounter = 0;
function ChatProvider({ children }) {
    const [state, dispatch] = (0, react_1.useReducer)(reducer_1.chatReducer, reducer_1.initialState);
    const stateRef = (0, react_1.useRef)(state);
    stateRef.current = state;
    // ── Bridge: extension host -> reducer ──
    (0, react_1.useEffect)(() => {
        function handleMessage(event) {
            const msg = event.data;
            switch (msg.type) {
                case 'init':
                    dispatch({ type: 'INIT', generating: msg.generating, currentModel: msg.currentModel });
                    break;
                case 'userMessage':
                    dispatch({ type: 'ADD_USER_MESSAGE', text: msg.text });
                    break;
                case 'generationStarted':
                    dispatch({ type: 'START_GENERATION' });
                    break;
                case 'text':
                    dispatch({ type: 'APPEND_TEXT', content: msg.content });
                    break;
                case 'toolStart':
                    dispatch({
                        type: 'TOOL_START',
                        id: `tool-${++_toolIdCounter}`,
                        name: msg.name,
                        args: msg.args,
                    });
                    break;
                case 'toolResult':
                    // Find the latest running tool in the last assistant message
                    {
                        const msgs = stateRef.current.messages;
                        const last = msgs[msgs.length - 1];
                        const runningTool = last?.toolCalls?.findLast((t) => t.status === 'running');
                        if (runningTool) {
                            dispatch({
                                type: 'TOOL_RESULT',
                                id: runningTool.id,
                                name: msg.name,
                                output: msg.output,
                                success: msg.success,
                                durationMs: msg.durationMs,
                            });
                        }
                    }
                    break;
                case 'tokens':
                    dispatch({ type: 'SET_TOKENS', prompt: msg.prompt, completion: msg.completion, total: msg.total });
                    break;
                case 'done':
                    dispatch({ type: 'GENERATION_DONE', tokens: msg.tokens });
                    break;
                case 'stopped':
                case 'generationStopped':
                    dispatch({ type: 'GENERATION_STOPPED' });
                    break;
                case 'error':
                    dispatch({ type: 'GENERATION_ERROR', message: msg.message });
                    break;
                case 'clearChat':
                    dispatch({ type: 'CLEAR_CHAT' });
                    break;
                case 'sessions':
                    dispatch({ type: 'SET_SESSIONS', sessions: msg.sessions });
                    break;
                case 'models':
                    dispatch({ type: 'SET_MODELS', models: msg.models });
                    break;
                case 'context':
                    dispatch({
                        type: 'ADD_CONTEXT_FILE',
                        file: {
                            path: msg.filePath,
                            fileName: msg.fileName,
                            language: msg.language,
                            selection: msg.selection,
                            type: msg.selection ? 'selection' : 'file',
                        },
                    });
                    break;
                case 'focusInput':
                    // Handled by a component, not state
                    break;
            }
        }
        window.addEventListener('message', handleMessage);
        // Signal the extension host that the webview is ready
        (0, vscode_1.postMessage)({ type: 'ready' });
        return () => window.removeEventListener('message', handleMessage);
    }, []);
    // ── Outbound actions ──
    const send = (0, react_1.useCallback)((text) => {
        const ctx = stateRef.current.contextFiles.length > 0
            ? stateRef.current.contextFiles.map((f) => ({ path: f.path, type: f.type }))
            : undefined;
        dispatch({ type: 'ADD_USER_MESSAGE', text, contextFiles: stateRef.current.contextFiles });
        (0, vscode_1.postMessage)({ type: 'send', text, context: ctx });
    }, []);
    const stop = (0, react_1.useCallback)(() => {
        (0, vscode_1.postMessage)({ type: 'stop' });
    }, []);
    const newConversation = (0, react_1.useCallback)(() => {
        (0, vscode_1.postMessage)({ type: 'newConversation' });
    }, []);
    const selectModel = (0, react_1.useCallback)((model) => {
        dispatch({ type: 'SET_CURRENT_MODEL', model });
        (0, vscode_1.postMessage)({ type: 'selectModel', model });
    }, []);
    const loadSession = (0, react_1.useCallback)((sessionId) => {
        (0, vscode_1.postMessage)({ type: 'loadSession', sessionId });
    }, []);
    const value = {
        state,
        dispatch,
        send,
        stop,
        newConversation,
        selectModel,
        loadSession,
    };
    return <ChatContext.Provider value={value}>{children}</ChatContext.Provider>;
}
//# sourceMappingURL=ChatProvider.js.map