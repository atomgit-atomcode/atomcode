// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App

import { useEffect, useRef, useState } from 'preact/hooks';
import { streamChat, SSEEvent, getSession, SessionMetaWithProject, getModels, ImageData } from '../api';
import { Markdown } from './Markdown';
import { ModelSelector } from './ModelSelector';
import { Suggestions } from './Suggestions';
import { AttachMenu } from './AttachMenu';
import { FilePicker } from './FilePicker';
import { useT } from '../settings';

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
  images?: ImageData[];
}

/** Max attached images per message and per-image byte cap (base64-decoded). */
const MAX_IMAGES = 6;
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;

/** Read a File into an ImageData (base64, no data-URL prefix). */
function fileToImageData(file: File): Promise<ImageData | null> {
  return new Promise((resolve) => {
    if (!file.type.startsWith('image/') || file.size > MAX_IMAGE_BYTES) {
      resolve(null);
      return;
    }
    const reader = new FileReader();
    reader.onload = () => {
      const result = String(reader.result || '');
      const comma = result.indexOf(',');
      resolve(comma >= 0 ? { media_type: file.type, data: result.slice(comma + 1) } : null);
    };
    reader.onerror = () => resolve(null);
    reader.readAsDataURL(file);
  });
}

/** Build a displayable data URL from an ImageData. */
function imageDataUrl(img: ImageData): string {
  return `data:${img.media_type};base64,${img.data}`;
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
  const t = useT();
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [tokens, setTokens] = useState<TokenUsage | null>(null);
  const [historyHint, setHistoryHint] = useState<string | null>(null);
  const [provider, setProvider] = useState<string | null>(null);
  const [showFilePicker, setShowFilePicker] = useState(false);
  const [pendingImages, setPendingImages] = useState<ImageData[]>([]);
  const abortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // 当前 Chat 正在显示的会话 id。用于区分「外部切换会话(需重置+加载历史)」
  // 与「本次新建会话首条消息完成后自己拿到的 id(不应重置)」。
  const activeIdRef = useRef<string | null>(null);
  // 已为哪个 sessionId 触发过历史加载（或它是本 Chat 自建的会话）。用于避免
  // project_hash 迟到（刷新后由 App 异步回填）导致的重复加载 / 覆盖当前对话。
  const loadedForRef = useRef<string | null>(null);

  // 切换/恢复会话时重置画布并加载历史。依赖 project_hash：刷新后 sessionId 先于
  // 元数据就绪，此时只显示提示；待 App 从会话列表回填 project_hash，本 effect 因
  // 依赖变化重跑，再真正拉取历史。
  useEffect(() => {
    // 会话 id 变化（外部切换 / 新建按钮）才重置画布。本 Chat 自建会话首条消息完成后
    // sessionId 变成自己的 id（activeIdRef 已同步），不重置，以免清空刚看到的对话。
    if (sessionId !== activeIdRef.current) {
      activeIdRef.current = sessionId;
      loadedForRef.current = null;
      abortRef.current?.abort();
      setBusy(false);
      setMessages([]);
      setTokens(null);
      setHistoryHint(null);
    }

    if (!sessionId) return;
    // 已为该会话加载过历史（或它是本 Chat 自建会话）→ 不重复加载、不覆盖。
    if (loadedForRef.current === sessionId) return;

    const projectHash = activeSession?.project_hash;
    if (!projectHash) {
      // 还没拿到 project_hash：先给「继续会话」提示，等其到位再由本 effect 重跑加载。
      setHistoryHint(t('chat.continueSession', { id: sessionId.slice(0, 8) }));
      return;
    }

    // 标记已为该会话发起加载，避免并发/重复。
    loadedForRef.current = sessionId;
    const loadId = sessionId;
    getSession(projectHash, loadId)
      .then((detail) => {
        // 加载期间用户可能已切走，确保结果仍对应当前会话。
        if (activeIdRef.current !== loadId) return;
        // Convert loaded messages to display format. Assistant tool_calls become
        // tool rows; tool-role messages fold their result into the matching row
        // (by call_id). System messages are skipped.
        const loaded: Message[] = [];
        for (const msg of detail.messages) {
          if (msg.role === 'user') {
            loaded.push({
              role: 'user',
              text: msg.content ?? '',
              tools: [],
              images: msg.images && msg.images.length ? msg.images : undefined,
            });
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
          setHistoryHint(null);
        } else {
          setHistoryHint(t('chat.continueSession', { id: loadId.slice(0, 8) }));
        }
      })
      .catch(() => {
        // 失败回退提示，并清掉标记以允许后续重试。
        if (activeIdRef.current === loadId) {
          loadedForRef.current = null;
          setHistoryHint(t('chat.continueSession', { id: loadId.slice(0, 8) }));
        }
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, activeSession?.project_hash]);

  // Initialize provider from default model
  useEffect(() => {
    getModels().then((ms) => {
      const def = ms.find((m) => m.is_default) ?? ms[0];
      if (def) setProvider((p) => p ?? def.provider);
    }).catch(() => {});
  }, []);

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
        // 标记这是本 Chat 自己产生的会话 id，避免下面的 useEffect 误把当前对话清空，
        // 并标记其历史「已就位」（就是当前画布），防止 project_hash 回填后重新加载覆盖。
        activeIdRef.current = event.session_id;
        loadedForRef.current = event.session_id;
        onSessionId(event.session_id);
        setBusy(false);
        break;

      case 'stopped':
        setBusy(false);
        break;

      case 'error':
        appendToLastAssistant('\n\n' + t('chat.error', { msg: event.message }));
        setBusy(false);
        break;

      default:
        // Ignore tool_batch, artifact_*, etc.
        break;
    }
  }

  async function sendMessage() {
    const text = input.trim();
    const images = pendingImages;
    if ((!text && images.length === 0) || busy) return;

    setInput('');
    setPendingImages([]);
    // 重置输入框高度：清空 value 不会复位之前 auto-resize 撑高的内联 height
    if (textareaRef.current) textareaRef.current.style.height = 'auto';
    setHistoryHint(null);
    setBusy(true);

    // Push user message + empty assistant placeholder
    setMessages((prev) => [
      ...prev,
      { role: 'user', text, tools: [], images: images.length ? images : undefined },
      { role: 'assistant', text: '', tools: [] },
    ]);

    const controller = new AbortController();
    abortRef.current = controller;

    try {
      const body = {
        message: text,
        ...(sessionId ? { session_id: sessionId } : {}),
        ...(cwd ? { working_dir: cwd } : {}),
        ...(provider ? { provider } : {}),
        ...(images.length ? { images } : {}),
      };

      await streamChat(body, handleEvent, controller.signal);
    } catch (err: unknown) {
      if (err instanceof Error && err.name === 'AbortError') {
        // User cancelled
      } else {
        const msg = err instanceof Error ? err.message : String(err);
        appendToLastAssistant('\n\n' + t('chat.connError', { msg }));
      }
      setBusy(false);
    } finally {
      abortRef.current = null;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    // 忽略 IME 组字阶段的回车（中文/日文等输入法选词确认），避免误发送。
    // `isComposing` 为现代浏览器标准属性，覆盖输入法组字状态。
    if (e.isComposing) return;
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

  // 落地页建议点击：把完整 prompt 填入输入框并聚焦（用户可编辑后再发送）。
  function fillInput(prompt: string) {
    setInput(prompt);
    requestAnimationFrame(() => {
      const ta = textareaRef.current;
      if (!ta) return;
      ta.focus();
      ta.style.height = 'auto';
      ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
      const len = ta.value.length;
      ta.setSelectionRange(len, len);
    });
  }

  // 在 textarea 光标处插入文本（skill 命令 / 文件路径），并复位高度、聚焦。
  function insertAtCursor(text: string) {
    const ta = textareaRef.current;
    if (!ta) {
      setInput((v) => v + text);
      return;
    }
    const start = ta.selectionStart ?? ta.value.length;
    const end = ta.selectionEnd ?? ta.value.length;
    const next = ta.value.slice(0, start) + text + ta.value.slice(end);
    setInput(next);
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      const pos = start + text.length;
      el.setSelectionRange(pos, pos);
      el.style.height = 'auto';
      el.style.height = Math.min(el.scrollHeight, 160) + 'px';
    });
  }

  // 文件选择器选中 → 插入绝对路径（前面光标非空白则补一个空格，末尾留空格）。
  function handlePickFile(path: string) {
    const ta = textareaRef.current;
    const start = ta?.selectionStart ?? input.length;
    const before = (ta?.value ?? input).slice(0, start);
    const needLead = before.length > 0 && !/\s$/.test(before);
    insertAtCursor((needLead ? ' ' : '') + path + ' ');
  }

  // 追加图片（上传或粘贴）：过滤非图片/超限，去除解析失败的，限制总数。
  async function addImageFiles(files: File[] | FileList) {
    const arr = Array.from(files).filter((f) => f.type.startsWith('image/'));
    if (arr.length === 0) return;
    const parsed = (await Promise.all(arr.map(fileToImageData))).filter(
      (x): x is ImageData => x !== null,
    );
    if (parsed.length === 0) return;
    setPendingImages((prev) => [...prev, ...parsed].slice(0, MAX_IMAGES));
  }

  function removePendingImage(idx: number) {
    setPendingImages((prev) => prev.filter((_, i) => i !== idx));
  }

  // 粘贴图片：从剪贴板提取图片文件（有图才拦截默认行为，纯文本粘贴不受影响）。
  function handlePaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const it of Array.from(items)) {
      if (it.kind === 'file' && it.type.startsWith('image/')) {
        const f = it.getAsFile();
        if (f) files.push(f);
      }
    }
    if (files.length) {
      e.preventDefault();
      addImageFiles(files);
    }
  }

  const lastIdx = messages.length - 1;

  // 新对话落地态：无会话、无消息、无历史提示 → claude.ai 风格的居中落地页。
  const landing = !sessionId && messages.length === 0 && !historyHint;

  // 落地页副标题：项目名 + 缩写路径。
  const cleanCwd = cwd.replace(/\/+$/, '');
  const cwdIdx = cleanCwd.lastIndexOf('/');
  const projName = cwdIdx >= 0 ? cleanCwd.slice(cwdIdx + 1) : cleanCwd;
  const projPath =
    cleanCwd.startsWith('/Users/') || cleanCwd.startsWith('/home/')
      ? '~/' + cleanCwd.split('/').slice(3).join('/')
      : cleanCwd;

  // 输入框只渲染一份，按落地/常规两处择一挂载（避免两个 textarea 抢同一 ref）。
  const inputBox = (
    <div class="input-box">
      {pendingImages.length > 0 && (
        <div class="input-thumbs">
          {pendingImages.map((img, i) => (
            <div key={i} class="input-thumb">
              <img src={imageDataUrl(img)} alt="" />
              <button
                class="input-thumb-remove"
                onClick={() => removePendingImage(i)}
                title={t('attach.removeImage')}
                aria-label={t('attach.removeImage')}
              >
                ×
              </button>
            </div>
          ))}
        </div>
      )}
      <textarea
        ref={textareaRef}
        class="message-input"
        rows={2}
        placeholder={t('chat.inputPlaceholder')}
        value={input}
        disabled={busy}
        onInput={handleInput}
        onKeyDown={handleKeyDown}
        onPaste={handlePaste}
      />
      <div class="input-footer">
        <AttachMenu
          onInsert={insertAtCursor}
          onPickFile={() => setShowFilePicker(true)}
          onAddImages={addImageFiles}
        />
        <span class="footer-spacer" />
        {tokens && (
          <span class="footer-tokens">
            {(tokens.total / 1000).toFixed(1)}k tokens
          </span>
        )}
        <ModelSelector value={provider} onChange={setProvider} />
        {busy ? (
          <button class="btn-stop" onClick={handleStop} title={t('chat.stop')} aria-label={t('chat.stop')}>
            <span class="stop-square" />
          </button>
        ) : (
          <button
            class="btn-send"
            onClick={sendMessage}
            disabled={!input.trim() && pendingImages.length === 0}
            title={t('chat.send')}
            aria-label={t('chat.send')}
          >
            ↑
          </button>
        )}
      </div>
    </div>
  );

  // 文件选择器模态（落地态与常规态共用一份）。
  const filePickerModal = showFilePicker && (
    <FilePicker
      current={cwd}
      onPick={handlePickFile}
      onClose={() => setShowFilePicker(false)}
    />
  );

  if (landing) {
    return (
      <>
        <div class="chat-landing">
          <div class="landing-inner">
            <div class="landing-brand">AtomCode</div>
            {cwd && (
              <div class="landing-subtitle">
                <span class="landing-project">{projName}</span>
                <span class="landing-path">{projPath}</span>
              </div>
            )}
            {inputBox}
            <Suggestions cwd={cwd} onPick={fillInput} />
          </div>
        </div>
        {filePickerModal}
      </>
    );
  }

  return (
    <>
      {/* Message timeline */}
      <div class="messages-container">
        {messages.length === 0 && !historyHint && (
          <div class="messages-empty">
            <div>
              {t('chat.startHint')}
            </div>
          </div>
        )}

        {messages.length === 0 && historyHint && (
          <div class="messages-empty">
            <div>
              {historyHint}
              <div class="sub">{t('chat.continueHint')}</div>
            </div>
          </div>
        )}

        {messages.map((msg, idx) => {
          const isLast = idx === lastIdx;
          if (msg.role === 'user') {
            return (
              <div key={idx} class="user-message-wrapper">
                <div class="user-message-bubble">
                  {msg.images && msg.images.length > 0 && (
                    <div class="msg-images">
                      {msg.images.map((img, i) => (
                        <img key={i} class="msg-image" src={imageDataUrl(img)} alt="" />
                      ))}
                    </div>
                  )}
                  {msg.text}
                </div>
              </div>
            );
          }

          const isError =
            msg.text.includes('[错误:') ||
            msg.text.includes('[连接错误:') ||
            msg.text.includes('[Error:') ||
            msg.text.includes('[Connection error:');
          const streaming = isLast && busy;
          // 终条且简短（无工具、单行）时，去掉多余的“时间线末端”橙点，只留一个起始点。
          const terse =
            isLast && !streaming && msg.tools.length === 0 && !msg.text.includes('\n');
          const dotClass = isError ? 'dot-error' : 'dot-brand';
          const cls =
            'timeline-message ' +
            dotClass +
            (streaming ? ' dot-blink' : '') +
            (isLast ? ' is-last' : '') +
            (terse ? ' is-terse' : '');

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
      <div class="input-container">{inputBox}</div>
      {filePickerModal}
    </>
  );
}

function ToolRowView({ tool }: { tool: ToolRow }) {
  const t = useT();
  const [expanded, setExpanded] = useState(false);

  let annotation: { cls: string; label: string } | null = null;
  if (tool.status === 'waiting_approval') {
    annotation = { cls: 'waiting', label: t('tool.waiting') };
  } else if (tool.status === 'pending') {
    annotation = { cls: 'pending', label: t('tool.running') };
  } else if (tool.status === 'done') {
    annotation = {
      cls: 'success',
      label: tool.duration_ms != null ? `${(tool.duration_ms / 1000).toFixed(2)}s` : t('tool.done'),
    };
  } else if (tool.status === 'error') {
    annotation = { cls: 'error', label: t('tool.failed') };
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
              <div class="tool-body-row-label">{t('tool.args')}</div>
              <div class="tool-body-row-content">{tool.args}</div>
            </div>
          )}
          {tool.output && (
            <div class="tool-body-row">
              <div class="tool-body-row-label">{t('tool.output')}</div>
              <div class="tool-body-row-content">{tool.output}</div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
