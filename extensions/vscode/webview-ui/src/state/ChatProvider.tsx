import React, { createContext, useContext, useReducer, useEffect, useCallback, useRef } from 'react';
import { ChatState, ChatAction, ExtensionMessage } from './types';
import { chatReducer, initialState } from './reducer';
import { postMessage } from '../vscode';

// ─── Context ────────────────────────────────────────────────────

interface ChatContextValue {
  state: ChatState;
  dispatch: React.Dispatch<ChatAction>;
  send: (text: string) => void;
  stop: () => void;
  newConversation: () => void;
  selectModel: (model: string) => void;
  loadSession: (sessionId: string) => void;
}

const ChatContext = createContext<ChatContextValue | null>(null);

export function useChatContext(): ChatContextValue {
  const ctx = useContext(ChatContext);
  if (!ctx) throw new Error('useChatContext must be used inside <ChatProvider>');
  return ctx;
}

// ─── Provider ───────────────────────────────────────────────────

let _toolIdCounter = 0;

export function ChatProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = useReducer(chatReducer, initialState);
  const stateRef = useRef(state);
  stateRef.current = state;

  // ── Bridge: extension host -> reducer ──
  useEffect(() => {
    function handleMessage(event: MessageEvent<ExtensionMessage>) {
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
    postMessage({ type: 'ready' });

    return () => window.removeEventListener('message', handleMessage);
  }, []);

  // ── Outbound actions ──
  const send = useCallback(
    (text: string) => {
      const ctx = stateRef.current.contextFiles.length > 0
        ? stateRef.current.contextFiles.map((f) => ({ path: f.path, type: f.type }))
        : undefined;
      dispatch({ type: 'ADD_USER_MESSAGE', text, contextFiles: stateRef.current.contextFiles });
      postMessage({ type: 'send', text, context: ctx });
    },
    [],
  );

  const stop = useCallback(() => {
    postMessage({ type: 'stop' });
  }, []);

  const newConversation = useCallback(() => {
    postMessage({ type: 'newConversation' });
  }, []);

  const selectModel = useCallback((model: string) => {
    dispatch({ type: 'SET_CURRENT_MODEL', model });
    postMessage({ type: 'selectModel', model });
  }, []);

  const loadSession = useCallback((sessionId: string) => {
    postMessage({ type: 'loadSession', sessionId });
  }, []);

  const value: ChatContextValue = {
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
