// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App

import { useEffect, useRef, useState } from 'preact/hooks';
import { streamChat, SSEEvent, getSession, SessionMetaWithProject } from '../api';

interface ToolRow {
  id: string;
  name: string;
  args: string;
  status: 'pending' | 'done' | 'error' | 'waiting_approval';
  duration_ms?: number;
}

interface Message {
  role: 'user' | 'assistant';
  text: string;
  tools: ToolRow[];
}

interface TokenUsage {
  prompt: number;
  completion: number;
  total: number;
}

interface PermissionRequestEvent {
  type: 'permission_request';
  session_id: string;
  tool_name: string;
  reason: string;
  call_id: string;
  arguments: unknown;
}

interface ChatProps {
  sessionId: string | null;
  onSessionId: (id: string) => void;
  cwd: string;
  onPermission: (req: PermissionRequestEvent) => void;
  /** Metadata of the currently-active session (for loading history) */
  activeSession?: SessionMetaWithProject | null;
}

function formatArgs(args: unknown): string {
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args);
  } catch {
    return String(args);
  }
}

function abbreviateArgs(args: string, maxLen = 60): string {
  if (args.length <= maxLen) return args;
  return args.slice(0, maxLen) + '…';
}

export function Chat({ sessionId, onSessionId, cwd, onPermission, activeSession }: ChatProps) {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [tokens, setTokens] = useState<TokenUsage | null>(null);
  const [historyHint, setHistoryHint] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // When sessionId changes from outside (sidebar switch or new session), reset transcript
  // and try to load session history
  useEffect(() => {
    // Abort any in-flight chat
    abortRef.current?.abort();
    setBusy(false);
    setMessages([]);
    setTokens(null);
    setHistoryHint(null);

    if (!sessionId) {
      // New session — blank slate
      return;
    }

    // Try to load history via session-detail endpoint
    if (activeSession?.project_hash) {
      getSession(activeSession.project_hash, sessionId)
        .then((detail) => {
          // Convert loaded messages to display format (user/assistant only; skip system/tool)
          const loaded: Message[] = [];
          for (const msg of detail.messages) {
            if (msg.role === 'user' || msg.role === 'assistant') {
              loaded.push({ role: msg.role, text: msg.content, tools: [] });
            }
          }
          if (loaded.length > 0) {
            setMessages(loaded);
          } else {
            setHistoryHint(`继续会话 ${sessionId.slice(0, 8)}（历史在 TUI/磁盘中）`);
          }
        })
        .catch(() => {
          // Endpoint exists but failed — show hint
          setHistoryHint(`继续会话 ${sessionId.slice(0, 8)}（历史在 TUI/磁盘中）`);
        });
    } else {
      setHistoryHint(`继续会话 ${sessionId.slice(0, 8)}（历史在 TUI/磁盘中）`);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // Auto-scroll to bottom when messages change
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages, tokens]);

  function appendToLastAssistant(content: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      return [
        ...prev.slice(0, -1),
        { ...last, text: last.text + content },
      ];
    });
  }

  function updateToolInLastAssistant(
    id: string,
    update: Partial<ToolRow>,
  ) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      const tools = last.tools.map((t) =>
        t.id === id ? { ...t, ...update } : t,
      );
      return [...prev.slice(0, -1), { ...last, tools }];
    });
  }

  function addToolToLastAssistant(tool: ToolRow) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      return [
        ...prev.slice(0, -1),
        { ...last, tools: [...last.tools, tool] },
      ];
    });
  }

  function handleEvent(event: SSEEvent) {
    switch (event.type) {
      case 'text':
        appendToLastAssistant(event.content);
        break;

      case 'tool_start': {
        const argsStr = formatArgs(event.arguments);
        addToolToLastAssistant({
          id: event.id,
          name: event.name,
          args: argsStr,
          status: 'pending',
        });
        break;
      }

      case 'tool_result':
        updateToolInLastAssistant(event.id, {
          status: event.success ? 'done' : 'error',
          duration_ms: event.duration_ms,
        });
        break;

      case 'tokens':
        setTokens({
          prompt: event.prompt,
          completion: event.completion,
          total: event.total,
        });
        break;

      case 'permission_request':
        // Mark the tool row as waiting for approval
        updateToolInLastAssistant(event.call_id, {
          status: 'waiting_approval',
        });
        onPermission(event as PermissionRequestEvent);
        break;

      case 'done':
        onSessionId(event.session_id);
        setBusy(false);
        break;

      case 'stopped':
        setBusy(false);
        break;

      case 'error':
        appendToLastAssistant(`\n\n[错误: ${event.message}]`);
        setBusy(false);
        break;

      default:
        // Ignore tool_batch, artifact_*, etc.
        break;
    }
  }

  async function sendMessage() {
    const text = input.trim();
    if (!text || busy) return;

    setInput('');
    setHistoryHint(null);
    setBusy(true);

    // Push user message + empty assistant placeholder
    setMessages((prev) => [
      ...prev,
      { role: 'user', text, tools: [] },
      { role: 'assistant', text: '', tools: [] },
    ]);

    const controller = new AbortController();
    abortRef.current = controller;

    try {
      const body = {
        message: text,
        ...(sessionId ? { session_id: sessionId } : {}),
        ...(cwd ? { working_dir: cwd } : {}),
      };

      await streamChat(body, handleEvent, controller.signal);
    } catch (err: unknown) {
      if (err instanceof Error && err.name === 'AbortError') {
        // User cancelled
      } else {
        const msg = err instanceof Error ? err.message : String(err);
        appendToLastAssistant(`\n\n[连接错误: ${msg}]`);
      }
      setBusy(false);
    } finally {
      abortRef.current = null;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  }

  function handleStop() {
    abortRef.current?.abort();
  }

  // Auto-resize textarea
  function handleInput(e: Event) {
    const ta = e.target as HTMLTextAreaElement;
    setInput(ta.value);
    ta.style.height = 'auto';
    ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
  }

  return (
    <div class="flex flex-col h-full">
      {/* Message area */}
      <div class="flex-1 overflow-y-auto p-6 space-y-5 min-h-0">
        {messages.length === 0 && !historyHint && (
          <div class="flex items-center justify-center h-full text-slate-400 text-sm">
            发送消息开始对话…
          </div>
        )}

        {messages.length === 0 && historyHint && (
          <div class="flex items-center justify-center h-full">
            <div class="text-center">
              <div class="text-slate-400 text-sm">{historyHint}</div>
              <div class="text-slate-300 text-xs mt-1">发送消息继续此会话</div>
            </div>
          </div>
        )}

        {messages.map((msg, idx) => (
          <div key={idx}>
            {msg.role === 'user' ? (
              <div class="flex justify-end">
                <div class="max-w-2xl px-4 py-2.5 rounded-2xl rounded-br-sm bg-indigo-600 text-white text-sm whitespace-pre-wrap">
                  {msg.text}
                </div>
              </div>
            ) : (
              <div class="flex flex-col gap-2">
                {/* Tool rows */}
                {msg.tools.map((tool) => (
                  <div class="flex justify-start" key={tool.id}>
                    <div class="max-w-2xl w-full">
                      <ToolRowView tool={tool} />
                    </div>
                  </div>
                ))}

                {/* Assistant text bubble — show even if empty while streaming */}
                {(msg.text || (idx === messages.length - 1 && busy)) && (
                  <div class="flex justify-start">
                    <div class="max-w-2xl px-4 py-3 rounded-2xl rounded-bl-sm bg-white border border-slate-200 text-sm leading-relaxed shadow-sm whitespace-pre-wrap">
                      {msg.text}
                      {idx === messages.length - 1 && busy && (
                        <span class="inline-block w-1.5 h-4 align-middle bg-slate-400 ml-0.5 animate-pulse" />
                      )}
                    </div>
                  </div>
                )}
              </div>
            )}
          </div>
        ))}

        <div ref={bottomRef} />
      </div>

      {/* Token usage bar */}
      {tokens && (
        <div class="px-6 py-1.5 text-xs text-slate-400 flex gap-4 border-t border-slate-100 bg-white/60">
          <span>↑ {(tokens.prompt / 1000).toFixed(1)}k prompt</span>
          <span>↓ {(tokens.completion / 1000).toFixed(1)}k completion</span>
          <span class="ml-auto">总计 {(tokens.total / 1000).toFixed(1)}k tokens</span>
        </div>
      )}

      {/* Input bar */}
      <div class="p-4 border-t border-slate-200 bg-white">
        <div class="flex items-end gap-2 rounded-xl border border-slate-300 focus-within:border-indigo-500 px-3 py-2">
          <textarea
            ref={textareaRef}
            rows={1}
            placeholder="输入消息，Enter 发送…"
            class="flex-1 resize-none outline-none text-sm bg-transparent"
            style="min-height: 24px; max-height: 160px;"
            value={input}
            disabled={busy}
            onInput={handleInput}
            onKeyDown={handleKeyDown}
          />
          {busy ? (
            <button
              onClick={handleStop}
              class="shrink-0 px-4 py-1.5 rounded-lg bg-slate-200 text-slate-700 text-sm font-medium hover:bg-slate-300"
            >
              停止
            </button>
          ) : (
            <button
              onClick={sendMessage}
              disabled={!input.trim()}
              class="shrink-0 px-4 py-1.5 rounded-lg bg-indigo-600 text-white text-sm font-medium hover:bg-indigo-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              发送
            </button>
          )}
        </div>
        <div class="mt-1.5 text-xs text-slate-400 flex gap-3">
          <span>Shift+Enter 换行</span>
          {cwd && <span class="ml-auto font-mono">cwd: {cwd}</span>}
        </div>
      </div>
    </div>
  );
}

function ToolRowView({ tool }: { tool: ToolRow }) {
  if (tool.status === 'waiting_approval') {
    return (
      <div class="flex items-center gap-2 px-3 py-2 rounded-lg bg-amber-50 border border-amber-300 text-xs font-mono">
        <span class="text-amber-500 animate-pulse">●</span>
        <span class="font-semibold text-slate-700">{tool.name}</span>
        <span class="text-slate-400 truncate">{abbreviateArgs(tool.args)}</span>
        <span class="ml-auto text-amber-600">等待批准…</span>
      </div>
    );
  }

  if (tool.status === 'pending') {
    return (
      <div class="flex items-center gap-2 px-3 py-2 rounded-lg bg-slate-100 border border-slate-200 text-xs font-mono">
        <span class="text-slate-400 animate-spin inline-block">⟳</span>
        <span class="font-semibold text-slate-700">{tool.name}</span>
        <span class="text-slate-400 truncate">{abbreviateArgs(tool.args)}</span>
        <span class="ml-auto text-slate-400">运行中…</span>
      </div>
    );
  }

  return (
    <div class="flex items-center gap-2 px-3 py-2 rounded-lg bg-slate-100 border border-slate-200 text-xs font-mono">
      <span class={tool.status === 'done' ? 'text-emerald-600' : 'text-rose-500'}>
        {tool.status === 'done' ? '✓' : '✗'}
      </span>
      <span class="font-semibold text-slate-700">{tool.name}</span>
      <span class="text-slate-400 truncate">{abbreviateArgs(tool.args)}</span>
      {tool.duration_ms != null && (
        <span class="ml-auto text-slate-400">
          耗时 {(tool.duration_ms / 1000).toFixed(2)}s
        </span>
      )}
    </div>
  );
}
