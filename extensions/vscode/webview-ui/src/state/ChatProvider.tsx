import React, { createContext, useContext, useReducer, useEffect, useCallback, useRef } from 'react';
import { ChatState, ChatAction, ExtensionMessage, ImageData, ApprovalMode } from './types';
import { chatReducer, initialState } from './reducer';
import { postMessage, getVSCodeApi } from '../vscode';
import { createTranslator } from '../i18n';
import { shouldShowIdleNotice } from '../utils/streamStatus';

// ─── Context ────────────────────────────────────────────────────

interface ChatContextValue {
  state: ChatState;
  dispatch: React.Dispatch<ChatAction>;
  send: (text: string, images?: ImageData[]) => void;
  stop: () => void;
  newConversation: () => void;
  selectModel: (provider: string, model?: string) => void;
  selectReasoningEffort: (provider: string, effort: string | null) => void;
  selectApprovalMode: (mode: ApprovalMode) => void;
  loadSession: (sessionId: string, projectHash?: string) => void;
  openSidebar: () => void;
  openSessionInTab: (sessionId?: string, projectHash?: string) => void;
  renameSession: (session: { id: string; project_hash?: string; name?: string; title?: string }) => void;
  deleteSession: (session: { id: string; project_hash?: string; name?: string; title?: string }) => void;
  deleteSessions: (sessions: Array<{ id: string; project_hash?: string; name?: string }>) => void;
  startLogin: () => void;
  cancelLogin: () => void;
  setupCodingPlan: () => void;
  refreshSetupState: () => void;
  setDefaultProvider: (name: string) => void;
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
  const lastStreamEventAtRef = useRef(Date.now());
  const idleNoticeShownRef = useRef(false);
  stateRef.current = state;

  // ── Bridge: extension host -> reducer ──
  useEffect(() => {
    function handleMessage(event: MessageEvent<ExtensionMessage>) {
      const msg = event.data;
      const markStreamActivity = () => {
        lastStreamEventAtRef.current = Date.now();
        idleNoticeShownRef.current = false;
      };
      switch (msg.type) {
        case 'chromeFont': {
          // Live-apply a chat-font config change without reloading the webview. Edit the SAME
          // `<style id="atomcode-chat-font">` rule the HTML injects (a stylesheet rule), so
          // clearing (`null`) truly reverts to the CSS default (`--vscode-editor-font-family`) —
          // an inline `documentElement.style.removeProperty` could not clear a stylesheet rule.
          // `msg.value` is already sanitized provider-side (see resolveChatFontFamily).
          let styleEl = document.getElementById('atomcode-chat-font');
          if (!styleEl) {
            styleEl = document.createElement('style');
            styleEl.id = 'atomcode-chat-font';
            document.head.appendChild(styleEl);
          }
          styleEl.textContent = msg.value
            ? `:root{--app-monospace-font-family:${msg.value};}`
            : '';
          break;
        }
        case 'init':
          dispatch({
            type: 'INIT',
            generating: msg.generating,
            recoveryLocked: msg.recoveryLocked,
            currentModel: msg.currentModel,
            viewMode: msg.viewMode,
            activeSessionId: msg.activeSessionId,
            projectHash: msg.projectHash,
            isSessionList: msg.isSessionList,
            locale: msg.locale,
            approvalMode: msg.approvalMode,
            approvalModePending: msg.approvalModePending,
          });
          // Persist session binding so tabs survive VS Code restart
          if (msg.activeSessionId) {
            getVSCodeApi().setState({ sessionId: msg.activeSessionId, projectHash: msg.projectHash });
          }
          break;
        case 'userMessage':
          dispatch({ type: 'ADD_USER_MESSAGE', text: msg.text, images: msg.images });
          break;
        case 'queuedMessageSent':
          dispatch({ type: 'SEND_QUEUED_MESSAGE', id: msg.id });
          break;
        case 'clearQueuedMessages':
          dispatch({ type: 'CLEAR_QUEUED_MESSAGES' });
          break;
        case 'assistantMessage':
          dispatch({ type: 'ADD_ASSISTANT_MESSAGE', text: msg.text });
          break;
        case 'generationStarted':
          markStreamActivity();
          dispatch({ type: 'START_GENERATION' });
          break;
        case 'text':
          markStreamActivity();
          dispatch({ type: 'APPEND_TEXT', content: msg.content });
          break;
        case 'toolBatchStart':
          markStreamActivity();
          dispatch({ type: 'TOOL_BATCH_START', calls: msg.calls });
          break;
        case 'toolStart':
          markStreamActivity();
          dispatch({
            type: 'TOOL_START',
            id: msg.id || `tool-${++_toolIdCounter}`,
            name: msg.name,
            args: msg.args,
          });
          break;
        case 'toolProgress':
          markStreamActivity();
          dispatch({ type: 'TOOL_PROGRESS', id: msg.id, progress: msg.progress });
          break;
        case 'toolResult':
          markStreamActivity();
          // Match tool by ID if provided, otherwise find the latest running tool
          {
            const msgs = stateRef.current.messages;
            const last = msgs[msgs.length - 1];
            const targetTool = msg.id
              ? last?.toolCalls?.find((t) => t.id === msg.id)
              : last?.toolCalls?.findLast((t) => t.status === 'running');
            if (targetTool) {
              dispatch({
                type: 'TOOL_RESULT',
                id: targetTool.id,
                name: msg.name,
                output: msg.output,
                success: msg.success,
                durationMs: msg.durationMs,
              });
            }
          }
          break;
        case 'warning':
          markStreamActivity();
          dispatch({ type: 'STREAM_WARNING', message: msg.message });
          break;
        case 'persistenceWarning':
          dispatch({ type: 'SET_PERSISTENCE_WARNING', message: msg.message });
          break;
        case 'rateLimited':
          markStreamActivity();
          dispatch({
            type: 'STREAM_RATE_LIMITED',
            message: msg.message,
            retryAfterSeconds: msg.retryAfterSeconds,
            attempt: msg.attempt,
            maxAttempts: msg.maxAttempts,
          });
          break;
        case 'artifactStart':
          markStreamActivity();
          dispatch({
            type: 'ARTIFACT_START',
            id: msg.id,
            artifactType: msg.artifactType,
            language: msg.language,
            title: msg.title,
          });
          break;
        case 'artifactContent':
          markStreamActivity();
          dispatch({ type: 'ARTIFACT_CONTENT', id: msg.id, content: msg.content });
          break;
        case 'artifactEnd':
          markStreamActivity();
          dispatch({ type: 'ARTIFACT_END', id: msg.id });
          break;
        case 'tokens':
          markStreamActivity();
          dispatch({ type: 'SET_TOKENS', prompt: msg.prompt, completion: msg.completion, total: msg.total });
          break;
        case 'done':
          markStreamActivity();
          if (msg.stopReason && msg.stopReason !== 'stopped') {
            const detail = msg.message || createTranslator(stateRef.current.locale)(
              'stream.incomplete',
              { reason: msg.stopReason },
            );
            dispatch({
              type: 'STREAM_WARNING',
              message: detail,
            });
          }
          dispatch({ type: 'GENERATION_DONE', tokens: msg.tokens });
          if (msg.sessionId) {
            dispatch({ type: 'SET_ACTIVE_SESSION', sessionId: msg.sessionId });
          }
          break;
        case 'stopped':
        case 'generationStopped':
          markStreamActivity();
          dispatch({ type: 'GENERATION_STOPPED' });
          break;
        case 'error':
          markStreamActivity();
          dispatch({ type: 'GENERATION_ERROR', message: msg.message });
          break;
        case 'recoveryRequired':
          dispatch({ type: 'RECOVERY_REQUIRED' });
          break;
        case 'recoveryCleared':
          dispatch({ type: 'RECOVERY_CLEARED' });
          break;
        case 'clearChat':
          dispatch({ type: 'CLEAR_CHAT' });
          break;
        case 'resumeStreaming':
          markStreamActivity();
          dispatch({ type: 'RESUME_STREAMING' });
          break;
        case 'sessions':
          dispatch({ type: 'SET_SESSIONS', sessions: msg.sessions });
          break;
        case 'sessionSelected':
          dispatch({ type: 'SET_ACTIVE_SESSION', sessionId: msg.sessionId, projectHash: msg.projectHash });
          getVSCodeApi().setState({ sessionId: msg.sessionId, projectHash: msg.projectHash });
          break;
        case 'models':
          dispatch({ type: 'SET_MODELS', models: msg.models });
          break;
        case 'approvalMode':
          dispatch({ type: 'SET_APPROVAL_MODE', mode: msg.mode, pending: msg.pending });
          break;
        case 'providers':
          dispatch({ type: 'SET_PROVIDERS', providers: msg.providers, defaultProvider: msg.defaultProvider });
          break;
        case 'runtimeInfo':
          dispatch({ type: 'SET_CURRENT_PROVIDER', provider: msg.provider, model: msg.model });
          break;
        case 'authStatus':
          dispatch({ type: 'SET_AUTH', auth: msg.auth });
          break;
        case 'setupState':
          dispatch({
            type: 'SET_SETUP_STATE',
            auth: msg.auth,
            providers: msg.providers,
            defaultProvider: msg.defaultProvider,
            currentModel: msg.currentModel,
            setupRequired: msg.setupRequired,
          });
          break;
        case 'loginStarted':
          dispatch({ type: 'SET_SETUP_STATUS', status: createTranslator(stateRef.current.locale)('setup.waitingForBrowser'), loginUrl: msg.url });
          break;
        case 'loginPending':
          dispatch({ type: 'SET_SETUP_STATUS', status: createTranslator(stateRef.current.locale)('setup.waitingForBrowser') });
          break;
        case 'loginAuthorized':
          dispatch({ type: 'SET_SETUP_STATUS', status: createTranslator(stateRef.current.locale)('setup.signedInNextStep') });
          break;
        case 'setupWorking':
          dispatch({ type: 'SET_SETUP_STATUS', status: msg.message });
          break;
        case 'codingPlanResult':
          dispatch({
            type: 'SET_SETUP_STATUS',
            status: msg.result.report_text,
          });
          break;
        case 'setupError':
          dispatch({ type: 'SET_SETUP_STATUS', error: msg.message });
          break;
        case 'sessionMessages':
          dispatch({
            type: 'LOAD_SESSION_MESSAGES',
            messages: msg.messages,
            terminal: msg.terminal,
            todos: msg.todos,
          });
          break;
        case 'context':
          dispatch({
            type: 'ADD_CONTEXT_FILE',
            file: {
              path: msg.filePath,
              fileName: msg.fileName,
              language: msg.language,
              selection: msg.selection,
              startLine: msg.startLine,
              endLine: msg.endLine,
              type: msg.selection ? 'selection' : 'file',
            },
          });
          break;
        case 'permissionRequest':
          markStreamActivity();
          dispatch({
            type: 'PERMISSION_REQUEST',
            id: msg.id,
            sessionId: msg.sessionId,
            toolName: msg.toolName,
            reason: msg.reason,
            args: msg.args,
            isDestructive: msg.isDestructive,
          });
          break;
        case 'permissionResponseResult':
          dispatch({
            type: 'PERMISSION_RESPONSE_RESULT',
            id: msg.id,
            success: msg.success,
            message: msg.message,
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

  useEffect(() => {
    const timer = window.setInterval(() => {
      const now = Date.now();
      if (!shouldShowIdleNotice({
        isGenerating: stateRef.current.isGenerating,
        lastEventAt: lastStreamEventAtRef.current,
        now,
        thresholdMs: 15_000,
        alreadyShown: idleNoticeShownRef.current,
      })) {
        return;
      }

      idleNoticeShownRef.current = true;
      dispatch({
        type: 'STREAM_IDLE_NOTICE',
        message: createTranslator(stateRef.current.locale)('stream.idleNotice'),
      });
    }, 1000);

    return () => window.clearInterval(timer);
  }, []);

  // ── Outbound actions ──
  const send = useCallback(
    (text: string, images?: ImageData[]) => {
      const state = stateRef.current;
      if (state.approvalModePending || state.recoveryLocked) return;
      const ctx = state.contextFiles.length > 0
        ? state.contextFiles.map((f) => ({
            path: f.path,
            type: f.type,
            fileName: f.fileName,
            language: f.language,
            selection: f.selection,
            startLine: f.startLine,
            endLine: f.endLine,
          }))
        : undefined;
      const contextFiles = state.contextFiles;
      const isQueued = state.isGenerating;
      const clientMessageId = `queued-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      if (isQueued) {
        dispatch({ type: 'ADD_QUEUED_MESSAGE', id: clientMessageId, text, contextFiles, images });
      } else {
        dispatch({ type: 'ADD_USER_MESSAGE', text, contextFiles, images });
      }
      postMessage({
        type: 'send',
        text,
        context: ctx,
        images,
        clientMessageId: isQueued ? clientMessageId : undefined,
        sessionId: state.activeSessionId,
        approvalMode: state.approvalMode,
      });
      // Clear context after sending
      dispatch({ type: 'CLEAR_CONTEXT' });
    },
    [],
  );

  const stop = useCallback(() => {
    postMessage({ type: 'stop', sessionId: stateRef.current.activeSessionId });
  }, []);

  const newConversation = useCallback(() => {
    postMessage({ type: 'newConversation' });
  }, []);

  const selectModel = useCallback((provider: string, model?: string) => {
    dispatch({ type: 'SET_CURRENT_PROVIDER', provider, model });
    postMessage({ type: 'selectModel', provider, model });
  }, []);

  const selectReasoningEffort = useCallback((provider: string, effort: string | null) => {
    dispatch({ type: 'SET_REASONING_EFFORT', provider, effort });
    postMessage({ type: 'selectReasoningEffort', provider, effort });
  }, []);

  const selectApprovalMode = useCallback((mode: ApprovalMode) => {
    if (stateRef.current.approvalModePending) return;
    postMessage({ type: 'selectApprovalMode', mode });
  }, []);

  const loadSession = useCallback((sessionId: string, projectHash?: string) => {
    postMessage({ type: 'loadSession', sessionId, projectHash });
  }, []);

  const openSidebar = useCallback(() => {
    postMessage({ type: 'openSidebar' });
  }, []);

  const openSessionInTab = useCallback((sessionId?: string, projectHash?: string) => {
    postMessage({ type: 'openSessionInTab', sessionId, projectHash });
  }, []);

  const renameSession = useCallback((session: { id: string; project_hash?: string; name?: string; title?: string }) => {
    postMessage({
      type: 'renameSession',
      sessionId: session.id,
      projectHash: session.project_hash,
      name: session.name || session.title || '',
    });
  }, []);

  const deleteSession = useCallback((session: { id: string; project_hash?: string; name?: string; title?: string }) => {
    postMessage({
      type: 'deleteSession',
      sessionId: session.id,
      projectHash: session.project_hash,
      name: session.name || session.title || '',
    });
  }, []);

  const deleteSessions = useCallback((sessions: Array<{ id: string; project_hash?: string; name?: string }>) => {
    postMessage({
      type: 'deleteSessions',
      sessions: sessions.map((s) => ({
        sessionId: s.id,
        projectHash: s.project_hash,
        name: s.name || '',
      })),
    });
  }, []);

  const startLogin = useCallback(() => {
    postMessage({ type: 'authLoginStart' });
  }, []);

  const cancelLogin = useCallback(() => {
    postMessage({ type: 'authLoginCancel' });
  }, []);

  const setupCodingPlan = useCallback(() => {
    postMessage({ type: 'codingPlanSetup' });
  }, []);

  const refreshSetupState = useCallback(() => {
    postMessage({ type: 'refreshSetupState' });
  }, []);

  const setDefaultProvider = useCallback((name: string) => {
    postMessage({ type: 'providerSetDefault', name });
  }, []);

  const value: ChatContextValue = {
    state,
    dispatch,
    send,
    stop,
    openSidebar,
    newConversation,
    selectModel,
    selectReasoningEffort,
    selectApprovalMode,
    loadSession,
    openSessionInTab,
    renameSession,
    deleteSession,
    deleteSessions,
    startLogin,
    cancelLogin,
    setupCodingPlan,
    refreshSetupState,
    setDefaultProvider,
  };

  return <ChatContext.Provider value={value}>{children}</ChatContext.Provider>;
}
