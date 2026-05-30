// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App

import { useEffect, useRef, useState } from 'preact/hooks';
import { streamChat, SSEEvent, getSession, SessionMetaWithProject } from '../api';
import { Markdown } from './Markdown';

interface ToolRow {
  id: string;
  name: string;
  args: string;
  status: 'pending' | 'done' | 'error' | 'waiting_approval';
  duration_ms?: number;
  output?: string;
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
          // Convert loaded messages to display format. Assistant tool_calls become
          // tool rows; tool-role messages fold their result into the matching row
          // (by call_id). System messages are skipped.
          const loaded: Message[] = [];
          for (const msg of detail.messages) {
            if (msg.role === 'user') {
              loaded.push({ role: 'user', text: msg.content ?? '', tools: [] });
            } else if (msg.role === 'assistant') {
              const tools: ToolRow[] = (msg.tool_calls ?? []).map((tc) => ({
                id: tc.id,
                name: tc.name,
                args: tc.arguments || tc.display || '',
                status: 'done' as const,
              }));
              loaded.push({ role: 'assistant', text: msg.content ?? '', tools });
            } else if (msg.role === 'tool' && msg.tool_result) {
              // Fold the tool result into its originating tool row.
              const result = msg.tool_result;
              for (let i = loaded.length - 1; i >= 0; i--) {
                const m = loaded[i];
                if (m.role !== 'assistant') continue;
                const row = m.tools.find((t) => t.id === result.call_id);
                if (row) {
                  row.output = result.summary;
                  row.status = result.success ? 'done' : 'error';
                  break;
                }
              }
            }
            // system messages: skip
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

  function appendToolOutput(chunk: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant' || last.tools.length === 0) return prev;
      const tools = last.tools.slice();
      const lastTool = tools[tools.length - 1];
      tools[tools.length - 1] = {
        ...lastTool,
        output: (lastTool.output ?? '') + chunk,
      };
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

      case 'tool_output':
        appendToolOutput(event.chunk);
        break;

      case 'tool_result':
        updateToolInLastAssistant(event.id, {
          status: event.success ? 'done' : 'error',
          duration_ms: event.duration_ms,
          output: event.output,
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

  const lastIdx = messages.length - 1;

  return (
    <>
      {/* Message timeline */}
      <div class="messages-container">
        {messages.length === 0 && !historyHint && (
          <div class="messages-empty">
            <div>
              发送消息开始对话…
            </div>
          </div>
        )}

        {messages.length === 0 && historyHint && (
          <div class="messages-empty">
            <div>
              {historyHint}
              <div class="sub">发送消息继续此会话</div>
            </div>
          </div>
        )}

        {messages.map((msg, idx) => {
          const isLast = idx === lastIdx;
          if (msg.role === 'user') {
            return (
              <div key={idx} class="user-message-wrapper">
                <div class="user-message-bubble">{msg.text}</div>
              </div>
            );
          }

          const isError = msg.text.includes('[错误:') || msg.text.includes('[连接错误:');
          const streaming = isLast && busy;
          const dotClass = isError ? 'dot-error' : 'dot-brand';
          const cls =
            'timeline-message ' +
            dotClass +
            (streaming ? ' dot-blink' : '') +
            (isLast ? ' is-last' : '');

          return (
            <div key={idx} class={cls}>
              {/* Tool rows */}
              {msg.tools.length > 0 && (
                <div class="tool-list">
                  {msg.tools.map((tool) => (
                    <ToolRowView key={tool.id} tool={tool} />
                  ))}
                </div>
              )}

              {/* Assistant text — show even if empty while streaming */}
              {(msg.text || streaming) && (
                isError ? (
                  <div class="error-message-content">
                    {msg.text}
                    {streaming && <span class="streaming-cursor" />}
                  </div>
                ) : (
                  <>
                    <Markdown content={msg.text} />
                    {streaming && <span class="streaming-cursor" />}
                  </>
                )
              )}
            </div>
          );
        })}

        <div ref={bottomRef} />
      </div>

      {/* Floating input */}
      <div class="input-container">
        <div class="input-box">
          <textarea
            ref={textareaRef}
            class="message-input"
            rows={1}
            placeholder="输入消息，Enter 发送，Shift+Enter 换行…"
            value={input}
            disabled={busy}
            onInput={handleInput}
            onKeyDown={handleKeyDown}
          />
          <div class="input-footer">
            <span class="footer-hint">Shift+Enter 换行</span>
            <span class="footer-spacer" />
            {tokens && (
              <span class="footer-tokens">
                {(tokens.total / 1000).toFixed(1)}k tokens
              </span>
            )}
            {busy ? (
              <button class="btn-stop" onClick={handleStop} title="停止" aria-label="停止">
                <span class="stop-square" />
              </button>
            ) : (
              <button
                class="btn-send"
                onClick={sendMessage}
                disabled={!input.trim()}
                title="发送"
                aria-label="发送"
              >
                ↑
              </button>
            )}
          </div>
        </div>
      </div>
    </>
  );
}

function ToolRowView({ tool }: { tool: ToolRow }) {
  const [expanded, setExpanded] = useState(false);

  let annotation: { cls: string; label: string } | null = null;
  if (tool.status === 'waiting_approval') {
    annotation = { cls: 'waiting', label: '等待批准…' };
  } else if (tool.status === 'pending') {
    annotation = { cls: 'pending', label: '运行中…' };
  } else if (tool.status === 'done') {
    annotation = {
      cls: 'success',
      label: tool.duration_ms != null ? `${(tool.duration_ms / 1000).toFixed(2)}s` : '完成',
    };
  } else if (tool.status === 'error') {
    annotation = { cls: 'error', label: '失败' };
  }

  const hasDetail = !!(tool.args || tool.output);

  return (
    <div class="tool-body">
      <div class="tool-header" onClick={() => setExpanded((e) => !e)}>
        <span class="tool-name">{tool.name}</span>
        <span class="tool-name-secondary">{abbreviateArgs(tool.args)}</span>
        {annotation && (
          <span class={'tool-annotation ' + annotation.cls}>{annotation.label}</span>
        )}
        {hasDetail && (
          <span class={'tool-chevron' + (expanded ? ' expanded' : '')}>▾</span>
        )}
      </div>
      {expanded && hasDetail && (
        <div class="tool-body-grid">
          {tool.args && (
            <div class="tool-body-row">
              <div class="tool-body-row-label">参数</div>
              <div class="tool-body-row-content">{tool.args}</div>
            </div>
          )}
          {tool.output && (
            <div class="tool-body-row">
              <div class="tool-body-row-label">输出</div>
              <div class="tool-body-row-content">{tool.output}</div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
