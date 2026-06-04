// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App

import { VNode } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { streamChat, SSEEvent, getSession, SessionMetaWithProject, getModels, ImageData, streamLive, postLiveMessage, postLivePermission, postLiveProvider, LiveWireEvent, SessionMessage, getSkills, SkillInfo, listDir } from '../api';
import { resolvePendingAfterDecision } from '../lib/pendingPermission';
import { Markdown } from './Markdown';
import { ModelSelector } from './ModelSelector';
import { AttachMenu } from './AttachMenu';
import { FilePicker } from './FilePicker';
import { PermissionCard } from './PermissionCard';
import { useT } from '../settings';

interface ToolRow {
  id: string;
  name: string;
  args: string;
  status: 'pending' | 'done' | 'error' | 'waiting_approval';
  duration_ms?: number;
  output?: string;
}

/** One ordered conversation segment: a run of assistant text, or one tool
 *  call. Storing parts in arrival order preserves the chronological
 *  text→tool→text→tool interleaving the LLM produced (matching the TUI),
 *  instead of collapsing every tool to the head of the message. */
type MsgPart =
  | { kind: 'text'; text: string }
  | { kind: 'tool'; tool: ToolRow };

interface Message {
  role: 'user' | 'assistant';
  parts: MsgPart[];
  images?: ImageData[];
}

/** Concatenate all text segments (error-detection, skill-title, etc.). */
function messageText(m: Message): string {
  return m.parts.reduce((acc, p) => (p.kind === 'text' ? acc + p.text : acc), '');
}

/** Whether a message contains any tool segments. */
function messageHasTools(m: Message): boolean {
  return m.parts.some((p) => p.kind === 'tool');
}

/** Max attached images per message and per-image byte cap (raw file size). */
const MAX_IMAGES = 6;
const MAX_IMAGE_MB = 2;
const MAX_IMAGE_BYTES = MAX_IMAGE_MB * 1024 * 1024;

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

/**
 * 去掉 daemon 为「非视觉主模型」注入的图片识别（VL）标注块——它只是给盲文本模型读图的
 * 内部上下文，不该显示在用户的输入气泡里（用户看到的应只是自己打的字 + 图片缩略图）。
 * 标注块由 daemon 追加在原文之后，与 `live_api.rs::preprocess_live_caption` /
 * `lib.rs::process_chat_request` 的格式耦合：`\n\n[图片内容（由 X 识别）]\n…` 或
 * `\n\n[图片识别失败]`（原文为空时无前导换行）。仅影响显示；存储/喂给模型的文本不变。
 */
function stripVisionAnnotation(text: string): string {
  const markers = ['\n\n[图片内容（由', '[图片内容（由', '\n\n[图片识别失败]', '[图片识别失败]'];
  let cut = -1;
  for (const m of markers) {
    const idx = text.indexOf(m);
    if (idx >= 0 && (cut < 0 || idx < cut)) cut = idx;
  }
  return cut >= 0 ? text.slice(0, cut).trimEnd() : text;
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
  /** 审批已被解决时通知 App 清掉 /chat 的审批卡片：传 call_id 仅在匹配时清（工具已执行），
   *  传 null 则无条件清（回合 done/stopped/error 或用户中止——此时不可能再有待批准项）。 */
  onPermissionResolved?: (callId: string | null) => void;
  /** Metadata of the currently-active session (for loading history) */
  activeSession?: SessionMetaWithProject | null;
  /** 刷新后正按 URL 短 id 还原会话；为 true 时抑制新建落地页，避免闪屏。 */
  restoring?: boolean;
}

function formatArgs(args: unknown): string {
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args);
  } catch {
    return String(args);
  }
}

// The VISIBLE truncation is done by CSS ellipsis at the real row width
// (.tool-name-secondary flexes to fill the row), so the preview length
// follows the screen/window width. This cap is only a DOM-size guard for
// pathological args (e.g. a tool fed a whole file); 1000 is far beyond any
// realistic single-row character count, so it never truncates before the
// screen edge — full args remain available by expanding the row.
function abbreviateArgs(args: string, maxLen = 1000): string {
  if (args.length <= maxLen) return args;
  return args.slice(0, maxLen) + '…';
}

// 识别「技能/文档型」用户消息：首个非空字符是 markdown 标题、且内容较长。
// TUI 调用 /skill 时会把整段 SKILL.md 模板塞进用户消息，webui 历史里会把它
// 渲染成一大坨原文；命中则返回标题文本用作折叠徽章标签，否则返回 null（普通气泡）。
const SKILL_COLLAPSE_MIN = 400;
function detectSkillContent(text: string): string | null {
  const trimmed = text.replace(/^\s+/, '');
  if (!trimmed.startsWith('#') || text.length < SKILL_COLLAPSE_MIN) return null;
  const firstLine = trimmed.split('\n', 1)[0];
  const title = firstLine.replace(/^#{1,6}\s*/, '').trim();
  return title || null;
}

export function Chat({ sessionId, onSessionId, cwd, onPermission, onPermissionResolved, activeSession, restoring }: ChatProps) {
  const t = useT();
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  // AI 执行中输入的消息排队于此，待当前回合 done 后依次自动发送（对齐 VSCode 插件）。
  const [queued, setQueued] = useState<{ id: number; text: string; images?: ImageData[] }[]>([]);
  const queueIdRef = useRef(0);
  const [tokens, setTokens] = useState<TokenUsage | null>(null);
  const [historyHint, setHistoryHint] = useState<string | null>(null);
  // 正在拉取某会话历史：用于抑制落地页，避免切到「有内容的会话」时先闪一下落地页。
  const [loading, setLoading] = useState(false);
  const [provider, setProvider] = useState<string | null>(null);
  const [showFilePicker, setShowFilePicker] = useState(false);
  const [pendingImages, setPendingImages] = useState<ImageData[]>([]);
  const [attachError, setAttachError] = useState<string | null>(null);
  const [slashSkills, setSlashSkills] = useState<SkillInfo[] | null>(null);
  const [slashLoading, setSlashLoading] = useState(false);
  const [slashOpen, setSlashOpen] = useState(false);
  const [slashQuery, setSlashQuery] = useState('');
  const [slashIndex, setSlashIndex] = useState(0);
  const [atOpen, setAtOpen] = useState(false);
  const [atQuery, setAtQuery] = useState('');
  const [atIndex, setAtIndex] = useState(0);
  const [atItems, setAtItems] = useState<{ name: string; is_dir: boolean }[]>([]);
  const [atLoading, setAtLoading] = useState(false);
  const [sync, setSync] = useState<boolean>(() => {
    try { return new URLSearchParams(location.search).get('sync') === '1'; } catch { return false; }
  });
  // Pending live-session permission request (shown as PermissionCard, calls /live/permission).
  // Kept separate from the non-sync `onPermission` prop so the /chat path is untouched.
  const [livePending, setLivePending] = useState<{ tool_name: string; reason: string; call_id: string; arguments: string } | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const liveAbortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const slashRef = useRef<HTMLDivElement>(null);
  const atRef = useRef<HTMLDivElement>(null);
  // 当前 Chat 正在显示的会话 id。用于区分「外部切换会话(需重置+加载历史)」
  // 与「本次新建会话首条消息完成后自己拿到的 id(不应重置)」。
  const activeIdRef = useRef<string | null>(null);
  // 已为哪个 sessionId 触发过历史加载（或它是本 Chat 自建的会话）。用于避免
  // project_hash 迟到（刷新后由 App 异步回填）导致的重复加载 / 覆盖当前对话。
  const loadedForRef = useRef<string | null>(null);
  // 实时（/live）总线对应的会话 id（来自 snapshot）。用于门控实时事件：仅当用户当前
  // 查看的就是这个实时会话时才把输出渲染进画布——否则用户从侧栏打开了别的历史会话，
  // 实时输出会串进错误页面、且刷新即消失（刷新会按真实会话重载）。
  const liveSessionIdRef = useRef<string | null>(null);

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
      // 切到一个有 id 的会话 → 进入「加载中」，先抑制落地页（避免闪屏）；
      // 无 id（新建）则不加载、直接落地。
      setLoading(sessionId != null);
    }

    if (!sessionId) return;
    // 已为该会话加载过历史（或它是本 Chat 自建会话）→ 不重复加载、不覆盖。
    if (loadedForRef.current === sessionId) return;

    const projectHash = activeSession?.project_hash;
    if (!projectHash) {
      // 还没拿到 project_hash：先给「继续会话」提示，等其到位再由本 effect 重跑加载。
      setHistoryHint(t('chat.continueSession', { id: sessionId.slice(0, 8) }));
      setLoading(false);
      return;
    }

    // 标记已为该会话发起加载，避免并发/重复。
    loadedForRef.current = sessionId;
    setLoading(true);
    const loadId = sessionId;
    getSession(projectHash, loadId)
      .then((detail) => {
        // 加载期间用户可能已切走，确保结果仍对应当前会话。
        if (activeIdRef.current !== loadId) return;
        // Convert loaded messages to display format (reuses sessionMessagesToDisplay).
        const loaded = sessionMessagesToDisplay(detail.messages);
        if (loaded.length > 0) {
          setMessages(loaded);
          setHistoryHint(null);
        } else {
          // 空会话：不再显示「继续会话」提示，交给落地页（landing）。
          setHistoryHint(null);
        }
        setLoading(false);
      })
      .catch(() => {
        // 失败回退提示，并清掉标记以允许后续重试。
        if (activeIdRef.current === loadId) {
          loadedForRef.current = null;
          setHistoryHint(t('chat.continueSession', { id: loadId.slice(0, 8) }));
        }
        setLoading(false);
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

  // Abort the live (/live) stream if the component unmounts while sync is on.
  useEffect(() => () => { liveAbortRef.current?.abort(); }, []);

  // 斜杠菜单：点击外部关闭
  useEffect(() => {
    if (!slashOpen) return;
    const h = (e: MouseEvent) => {
      if (slashRef.current && !slashRef.current.contains(e.target as Node)) {
        setSlashOpen(false);
      }
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [slashOpen]);

  // @ 菜单：点击外部关闭
  useEffect(() => {
    if (!atOpen) return;
    const h = (e: MouseEvent) => {
      if (atRef.current && !atRef.current.contains(e.target as Node)) {
        setAtOpen(false);
      }
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [atOpen]);

  // @ 文件菜单：把 @ 后文本拆成「目录段 + 过滤词」，以支持进入子目录 / 返回上级。
  // 例如 "examples/fo" → 列 cwd/examples 的内容，并按前缀 "fo" 过滤。
  const atSlash = atQuery.lastIndexOf('/');
  const atDirPart = atSlash >= 0 ? atQuery.slice(0, atSlash + 1) : '';
  const atFilter = atSlash >= 0 ? atQuery.slice(atSlash + 1) : atQuery;
  const atTargetDir =
    atDirPart === ''
      ? cwd
      : atDirPart.startsWith('/') || atDirPart.startsWith('~')
        ? atDirPart
        : cwd.replace(/\/+$/, '') + '/' + atDirPart;

  // 目录变化（cwd 切换 / 进入子目录）时重新拉取；仅过滤词变化不触发。后端会 canonicalize `..`。
  useEffect(() => {
    if (!atOpen) return;
    let cancelled = false;
    setAtLoading(true);
    listDir(atTargetDir)
      .then((r) => {
        if (cancelled) return;
        const items: { name: string; is_dir: boolean }[] = [];
        for (const d of r.dirs) items.push({ name: d, is_dir: true });
        if (r.files) for (const f of r.files) items.push({ name: f, is_dir: false });
        items.sort((a, b) => a.name.localeCompare(b.name));
        setAtItems(items);
      })
      .catch(() => { if (!cancelled) setAtItems([]); })
      .finally(() => { if (!cancelled) setAtLoading(false); });
    return () => { cancelled = true; };
  }, [atOpen, atTargetDir]);

  // 菜单可见行：进入子目录后首行为「..」返回上级（仅无过滤词时展示）；其余按过滤词前缀匹配。
  const atRows: { name: string; is_dir: boolean; up?: boolean }[] = [];
  if (atDirPart && atFilter === '') atRows.push({ name: '..', is_dir: true, up: true });
  for (const it of atItems) {
    if (it.name.toLowerCase().startsWith(atFilter.toLowerCase())) atRows.push(it);
  }

  // ── 共享的实时流启/停逻辑 ──
  function startLiveStream() {
    const controller = new AbortController();
    liveAbortRef.current = controller;
    streamLive(onLiveEvent, controller.signal, activeIdRef.current).catch(() => {
      // Stream ended or errored; turn sync back off
      setSync(false);
    });
  }

  function stopLiveStream() {
    liveAbortRef.current?.abort();
    liveAbortRef.current = null;
  }

  // 若 sync 初始值为 true（URL 带 sync=1），在挂载时自动连接实时流。
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (sync) {
      startLiveStream();
    }
    // 仅在挂载时执行一次；后续由 toggleSync 控制。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Shared history → display conversion (reused by session load AND live snapshot) ──
  function sessionMessagesToDisplay(msgs: SessionMessage[]): Message[] {
    const loaded: Message[] = [];
    for (const msg of msgs) {
      if (msg.role === 'user') {
        loaded.push({
          role: 'user',
          parts: [{ kind: 'text', text: stripVisionAnnotation(msg.content ?? '') }],
          images: msg.images && msg.images.length ? msg.images : undefined,
        });
      } else if (msg.role === 'assistant') {
        // Text comes first (the LLM speaks, then calls tools), so the part
        // order for a persisted round is [text, tool, tool, …].
        const parts: MsgPart[] = [];
        if (msg.content) parts.push({ kind: 'text', text: msg.content });
        for (const tc of msg.tool_calls ?? []) {
          parts.push({
            kind: 'tool',
            tool: {
              id: tc.id,
              name: tc.name,
              args: tc.arguments || tc.display || '',
              status: 'done',
            },
          });
        }
        loaded.push({ role: 'assistant', parts });
      } else if (msg.role === 'tool' && msg.tool_result) {
        const result = msg.tool_result;
        outer: for (let i = loaded.length - 1; i >= 0; i--) {
          const m = loaded[i];
          if (m.role !== 'assistant') continue;
          for (const p of m.parts) {
            if (p.kind === 'tool' && p.tool.id === result.call_id) {
              p.tool.output = result.summary;
              p.tool.status = result.success ? 'done' : 'error';
              break outer;
            }
          }
        }
      }
      // system messages: skip
    }
    return loaded;
  }

  // ── Live SSE adapter: map LiveWireEvent → SSEEvent (for variants that overlap) ──
  function liveToSSE(e: LiveWireEvent): SSEEvent | null {
    switch (e.type) {
      case 'text': return { type: 'text', content: e.content };
      case 'reasoning': return { type: 'reasoning', content: e.content };
      case 'tool_start': return { type: 'tool_start', id: e.id, name: e.name, arguments: e.arguments };
      case 'tool_output': return { type: 'tool_output', chunk: e.chunk };
      case 'tool_result': return { type: 'tool_result', id: e.id, name: e.name, output: e.output, success: e.success, duration_ms: e.duration_ms };
      case 'tokens': return { type: 'tokens', prompt: e.prompt, completion: e.completion, total: e.total };
      case 'error': return { type: 'error', message: e.message };
      default: return null;
    }
  }

  // ── Live event handler ──
  function onLiveEvent(e: LiveWireEvent) {
    // snapshot：确立实时会话 id 并把视图切到它（连上即对齐）。
    if (e.type === 'snapshot') {
      liveSessionIdRef.current = e.session_id || null;
      const loaded = sessionMessagesToDisplay(e.messages);
      setMessages(loaded.length > 0 ? loaded : []);
      setHistoryHint(null);
      // 连上时回显当前生效的模型，让下拉框与 TUI / 其他端保持一致。
      if (e.provider) setProvider(e.provider);
      // 把稳定的 session_id 告知 App，接入侧边栏历史 + URL 刷新恢复。
      // 与 /chat 的 'done' 事件同路径：activeIdRef + loadedForRef 标记，
      // 避免 App 回填 project_hash 时触发重复加载覆盖当前画布。
      if (e.session_id) {
        activeIdRef.current = e.session_id;
        loadedForRef.current = e.session_id;
        onSessionId(e.session_id);
      }
      return;
    }
    // 模型切换是进程级（全局），与正在查看哪个会话无关 → 不门控，始终更新下拉框。
    if (e.type === 'provider') {
      setProvider(e.provider);
      return;
    }

    // 门控：仅当"当前查看的会话"就是实时会话时，才把实时输出渲染进画布。否则用户
    // 从侧栏打开了另一个历史会话，实时事件不应串进该页面（串进去刷新还会消失）。
    if (
      liveSessionIdRef.current &&
      activeIdRef.current &&
      activeIdRef.current !== liveSessionIdRef.current
    ) {
      return;
    }

    switch (e.type) {
      case 'user': {
        // Append the peer's user message + empty assistant placeholder
        setMessages((prev) => [
          ...prev,
          { role: 'user', parts: [{ kind: 'text', text: e.text }], images: e.images && e.images.length ? e.images : undefined },
          { role: 'assistant', parts: [] },
        ]);
        break;
      }
      case 'state': {
        setBusy(e.running);
        // 回合结束（idle）时不可能再有待批准项：清掉因对端(TUI)批准或回合收尾而
        // 残留的审批卡片，否则 webui 会一直挂着一张「等待批准…」的卡片直到刷新。
        if (!e.running) setLivePending(null);
        break;
      }
      case 'permission_request': {
        // Mark the tool row as waiting for approval (same as non-sync path)
        updateToolInLastAssistant(e.call_id, { status: 'waiting_approval' });
        // Show the PermissionCard for the live session (calls /live/permission via onDecide)
        setLivePending({ tool_name: e.tool_name, reason: e.reason, call_id: e.call_id, arguments: e.arguments });
        break;
      }
      default: {
        const mapped = liveToSSE(e);
        if (mapped) handleEvent(mapped);
        // 工具结果到达即代表该工具的审批已被处理（本端或对端 TUI 批准后工具已执行），
        // 清掉与之对应的残留审批卡片（call_id 匹配才清，避免误删尚未处理的其它请求）。
        if (e.type === 'tool_result') {
          setLivePending((cur) => resolvePendingAfterDecision(cur, e.id));
        }
        break;
      }
    }
  }

  // ── Sync toggle: start / stop the live stream ──
  function toggleSync() {
    setSync((prev) => {
      const next = !prev;
      if (next) {
        startLiveStream();
      } else {
        stopLiveStream();
      }
      return next;
    });
  }

  function appendToLastAssistant(content: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      const parts = last.parts.slice();
      const tail = parts[parts.length - 1];
      if (tail && tail.kind === 'text') {
        // Continue the current text run.
        parts[parts.length - 1] = { kind: 'text', text: tail.text + content };
      } else {
        // First text, or text after a tool → start a new text segment so the
        // chronological order (…tool → text…) is preserved.
        parts.push({ kind: 'text', text: content });
      }
      return [...prev.slice(0, -1), { ...last, parts }];
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
      const parts = last.parts.map((p) =>
        p.kind === 'tool' && p.tool.id === id
          ? { kind: 'tool' as const, tool: { ...p.tool, ...update } }
          : p,
      );
      return [...prev.slice(0, -1), { ...last, parts }];
    });
  }

  function appendToolOutput(chunk: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      // Append to the most recent tool segment's output.
      let idx = -1;
      for (let i = last.parts.length - 1; i >= 0; i--) {
        if (last.parts[i].kind === 'tool') {
          idx = i;
          break;
        }
      }
      if (idx < 0) return prev;
      const parts = last.parts.slice();
      const tp = parts[idx] as { kind: 'tool'; tool: ToolRow };
      parts[idx] = {
        kind: 'tool',
        tool: { ...tp.tool, output: (tp.tool.output ?? '') + chunk },
      };
      return [...prev.slice(0, -1), { ...last, parts }];
    });
  }

  function addToolToLastAssistant(tool: ToolRow) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      return [
        ...prev.slice(0, -1),
        { ...last, parts: [...last.parts, { kind: 'tool', tool }] },
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
        // 工具已执行完 → 其审批必已解决，清掉 /chat 残留的同 call_id 审批卡片。
        onPermissionResolved?.(event.id);
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
        onPermissionResolved?.(null); // 回合结束：兜底清掉任何残留审批卡片
        break;

      case 'stopped':
        setBusy(false);
        setQueued([]); // 用户中止：丢弃排队消息（对齐 VSCode 插件）
        onPermissionResolved?.(null);
        break;

      case 'error':
        appendToLastAssistant('\n\n' + t('chat.error', { msg: event.message }));
        setBusy(false);
        setQueued([]); // 出错：丢弃排队消息
        onPermissionResolved?.(null);
        break;

      default:
        // Ignore tool_batch, artifact_*, etc.
        break;
    }
  }

  // 实际投递一条消息（同步 / 常规两条路径）；busy 由各自的事件流复位。
  async function deliver(text: string, images: ImageData[]) {
    if (sync) {
      // ── Sync path: send to /live/message; do NOT locally append (the user
      //    event will arrive back via the live stream, keeping all tabs in sync).
      setBusy(true);
      await postLiveMessage(text, images.length ? images : undefined, provider ?? undefined, activeIdRef.current);
      return;
    }

    // ── Normal path ──
    setBusy(true);

    // Push user message + empty assistant placeholder
    setMessages((prev) => [
      ...prev,
      { role: 'user', parts: [{ kind: 'text', text }], images: images.length ? images : undefined },
      { role: 'assistant', parts: [] },
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
      setQueued([]); // 连接错误：与 stopped/error 一致，丢弃排队消息
      // 中止/连接错误时流被掐断，不会再有 done/stopped 事件 → 兜底清掉审批卡片，
      // 否则点「停止」时若正挂着审批卡片，它会一直残留。
      onPermissionResolved?.(null);
    } finally {
      abortRef.current = null;
    }
  }

  function sendMessage() {
    const text = input.trim();
    const images = pendingImages;
    if (!text && images.length === 0) return;

    // 清空输入框（无论立即发送还是排队）。
    setInput('');
    setPendingImages([]);
    // 重置输入框高度：清空 value 不会复位之前 auto-resize 撑高的内联 height
    if (textareaRef.current) textareaRef.current.style.height = 'auto';
    setHistoryHint(null);

    // AI 执行中：排队，待当前回合 done 后由 drain effect 依次自动发送。
    if (busy) {
      setQueued((q) => [
        ...q,
        { id: queueIdRef.current++, text, images: images.length ? images : undefined },
      ]);
      return;
    }

    void deliver(text, images);
  }

  // 当前回合结束(done)后，依次发送排队消息；stopped/error/连接错误已清空队列。
  useEffect(() => {
    if (busy || queued.length === 0) return;
    const next = queued[0];
    setQueued((q) => q.slice(1));
    void deliver(next.text, next.images ?? []);
    // deliver 为组件内函数声明，闭包始终取最新渲染值；仅以 busy/queued 触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busy, queued]);

  function handleKeyDown(e: KeyboardEvent) {
    if (e.isComposing) return;

    // 斜杠菜单导航
    if (slashOpen) {
      const filtered = (slashSkills ?? []).filter((s) => s.name.toLowerCase().includes(slashQuery.toLowerCase())).sort((a, b) => a.name.localeCompare(b.name));
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSlashIndex((i) => Math.min(i + 1, filtered.length - 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSlashIndex((i) => Math.max(i - 1, 0));
        return;
      }
      if (e.key === 'Enter' && filtered.length > 0) {
        e.preventDefault();
        insertSkill(filtered[slashIndex].name);
        return;
      }
      if (e.key === 'Escape') {
        setSlashOpen(false);
        return;
      }
    }

    // @ 菜单导航
    if (atOpen) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setAtIndex((i) => Math.min(i + 1, atRows.length - 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setAtIndex((i) => Math.max(i - 1, 0));
        return;
      }
      // Enter/Tab：目录→进入，文件→选定。Tab 便于逐级深入。
      if ((e.key === 'Enter' || e.key === 'Tab') && atRows.length > 0) {
        e.preventDefault();
        chooseAtRow(atRows[Math.min(atIndex, atRows.length - 1)]);
        return;
      }
      if (e.key === 'Escape') {
        setAtOpen(false);
        return;
      }
    }

    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  }

  function handleStop() {
    abortRef.current?.abort();
  }

  // 从光标前的 / 替换为选中的技能名。
  function insertSkill(name: string) {
    const ta = textareaRef.current;
    if (!ta) return;
    const pos = ta.selectionStart ?? ta.value.length;
    const before = ta.value.slice(0, pos);
    const after = ta.value.slice(pos);
    const slashIdx = before.lastIndexOf('/');
    const next = before.slice(0, slashIdx) + `/${name} ` + after;
    setInput(next);
    setSlashOpen(false);
    requestAnimationFrame(() => {
      ta.focus();
      const newPos = slashIdx + name.length + 2;
      ta.setSelectionRange(newPos, newPos);
      ta.style.height = 'auto';
      ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
    });
  }

  // 把光标前的 @ 段替换为相对路径 rel。keepOpen=true 用于进入目录（保留菜单、继续浏览），
  // false 用于最终选定文件（补空格、关闭菜单）。
  function setAtMention(rel: string, keepOpen: boolean) {
    const ta = textareaRef.current;
    if (!ta) return;
    const pos = ta.selectionStart ?? ta.value.length;
    const before = ta.value.slice(0, pos);
    const after = ta.value.slice(pos);
    const atIdx = before.lastIndexOf('@');
    if (atIdx < 0) return;
    const suffix = keepOpen ? '' : ' ';
    const next = before.slice(0, atIdx) + `@${rel}${suffix}` + after;
    setInput(next);
    if (keepOpen) {
      setAtQuery(rel);
      setAtIndex(0);
    } else {
      setAtOpen(false);
    }
    requestAnimationFrame(() => {
      ta.focus();
      const newPos = atIdx + 1 + rel.length + suffix.length;
      ta.setSelectionRange(newPos, newPos);
      ta.style.height = 'auto';
      ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
    });
  }

  // 选择 @ 菜单某一行：「..」→返回上级；目录→进入；文件→插入完整相对路径并关闭。
  function chooseAtRow(row: { name: string; is_dir: boolean; up?: boolean }) {
    if (row.up) {
      const trimmed = atDirPart.replace(/\/+$/, '');
      const idx = trimmed.lastIndexOf('/');
      setAtMention(idx >= 0 ? trimmed.slice(0, idx + 1) : '', true);
    } else if (row.is_dir) {
      setAtMention(atDirPart + row.name + '/', true);
    } else {
      setAtMention(atDirPart + row.name, false);
    }
  }

  // Auto-resize textarea + slash-command + @-mention detection
  function handleInput(e: Event) {
    const ta = e.target as HTMLTextAreaElement;
    const val = ta.value;
    setInput(val);
    ta.style.height = 'auto';
    ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';

    const pos = ta.selectionStart ?? val.length;
    const before = val.slice(0, pos);

    // 检测光标前是否有 /（行首 或 空格后）
    const slashIdx = before.lastIndexOf('/');
    if (slashIdx >= 0 && (slashIdx === 0 || before[slashIdx - 1] === ' ')) {
      const query = before.slice(slashIdx + 1);
      if (!query.includes(' ') && query.length <= 30) {
        if (slashSkills === null && !slashLoading) {
          setSlashLoading(true);
          getSkills()
            .then(setSlashSkills)
            .catch(() => setSlashSkills([]))
            .finally(() => setSlashLoading(false));
        }
        setAtOpen(false);
        setSlashQuery(query);
        setSlashIndex(0);
        setSlashOpen(true);
        return;
      }
    }

    // 检测光标前是否有 @（行首 或 空格后）。@ 后文本可含 "/" 以进入子目录；
    // 实际列目录/过滤由派生的 atTargetDir + useEffect 处理（见上）。
    const atIdx = before.lastIndexOf('@');
    if (atIdx >= 0 && (atIdx === 0 || before[atIdx - 1] === ' ')) {
      const query = before.slice(atIdx + 1);
      if (!query.includes(' ') && query.length <= 120) {
        setSlashOpen(false);
        setAtQuery(query);
        setAtIndex(0);
        setAtOpen(true);
        return;
      }
    }

    setSlashOpen(false);
    setAtOpen(false);
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
    // 严格拦截超过 2M 的图片，并提示用户（其余正常入列）。
    const oversized = arr.filter((f) => f.size > MAX_IMAGE_BYTES);
    if (oversized.length > 0) {
      setAttachError(t('attach.tooLarge', { mb: String(MAX_IMAGE_MB) }));
    } else {
      setAttachError(null);
    }
    const allowed = arr.filter((f) => f.size <= MAX_IMAGE_BYTES);
    if (allowed.length === 0) return;
    const parsed = (await Promise.all(allowed.map(fileToImageData))).filter(
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

  // 落地态：对话为空就用 claude.ai 风格的居中落地页（无论是否已有 session id —
  // 新建会话、空的同步会话、空的历史会话都适用）。
  // 抑制条件：正在拉历史（loading，避免切到有内容会话时闪屏）、restoring（刷新还原中）、
  // 已有 historyHint（无法加载、提示去 TUI/磁盘续聊）。
  const landing = messages.length === 0 && !historyHint && !restoring && !loading;

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
      {attachError && (
        <div class="input-attach-error" role="alert">
          <span>{attachError}</span>
          <button
            class="input-attach-error-close"
            onClick={() => setAttachError(null)}
            aria-label={t('attach.dismissError')}
          >
            ×
          </button>
        </div>
      )}
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
      {atOpen && (
        <div class="at-popover" ref={atRef}>
          {atLoading && <div class="at-loading">Loading...</div>}
          {!atLoading && atRows.map((item, i) => (
            <button
              key={(item.up ? 'up:' : item.is_dir ? 'd:' : 'f:') + item.name}
              class={'at-row' + (i === atIndex ? ' active' : '')}
              onMouseDown={(e) => { e.preventDefault(); chooseAtRow(item); }}
              onMouseEnter={() => setAtIndex(i)}
              type="button"
              title={item.up ? '..' : atDirPart + item.name}
            >
              <span class="at-icon">{item.up ? '⬆' : item.is_dir ? '📁' : '📄'}</span>
              <span class="at-name">{item.up ? '..' : item.name}</span>
            </button>
          ))}
          {!atLoading && atRows.length === 0 && (
            <div class="at-empty">No files found</div>
          )}
        </div>
      )}
      {slashOpen && (
        <div class="slash-popover" ref={slashRef}>
          {(slashSkills ?? []).filter((s) => s.name.toLowerCase().includes(slashQuery.toLowerCase())).sort((a, b) => a.name.localeCompare(b.name)).map((s, i) => (
            <button
              key={s.name}
              class={'slash-row' + (i === slashIndex ? ' active' : '')}
              onMouseDown={(e) => { e.preventDefault(); insertSkill(s.name); }}
              onMouseEnter={() => setSlashIndex(i)}
              type="button"
              title={s.description || ''}
            >
              <span class="slash-name">/{s.name}</span>
              {s.description && <span class="slash-desc">{s.description}</span>}
            </button>
          ))}
        </div>
      )}
      <textarea
        ref={textareaRef}
        class="message-input"
        rows={2}
        placeholder={t('chat.inputPlaceholder')}
        value={input}
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
        <button
          class={'btn-sync' + (sync ? ' active' : '')}
          onClick={toggleSync}
          title={sync ? t('sync.on') : t('sync.off')}
          aria-label={t('sync.toggle')}
          aria-pressed={sync}
        >
          ⇄
        </button>
        <span class="footer-spacer" />
        {tokens && (
          <span class="footer-tokens">
            {(tokens.total / 1000).toFixed(1)}k tokens
          </span>
        )}
        <ModelSelector
          value={provider}
          onChange={(p) => {
            setProvider(p);
            // 同步模式：下拉框一变就通知后端，TUI 头部与其他端实时跟随
            // （非同步模式只改本端的待发 provider，发消息时再带上）。
            if (sync) void postLiveProvider(p);
          }}
        />
        {busy ? (
          <>
            {/* 执行中仍可发送：按下即排队，当前回合结束后自动发出。 */}
            {(input.trim() || pendingImages.length > 0) && (
              <button
                class="btn-send"
                onClick={sendMessage}
                title={t('chat.queue')}
                aria-label={t('chat.queue')}
              >
                ↑
              </button>
            )}
            <button class="btn-stop" onClick={handleStop} title={t('chat.stop')} aria-label={t('chat.stop')}>
              <span class="stop-square" />
            </button>
          </>
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

  // Live-session PermissionCard: shown when in sync mode and a permission_request arrives.
  // Uses onDecide to call /live/permission instead of /chat/permission.
  const livePermissionCard = livePending && (
    <PermissionCard
      req={{ session_id: '', tool_name: livePending.tool_name, reason: livePending.reason, call_id: livePending.call_id, arguments: livePending.arguments }}
      onDone={() => setLivePending((cur) => resolvePendingAfterDecision(cur, livePending.call_id))}
      onDecide={async (decision) => { await postLivePermission(decision); }}
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
          </div>
        </div>
        {filePickerModal}
        {livePermissionCard}
      </>
    );
  }

  return (
    <>
      {/* Message timeline */}
      <div class="messages-container">
        {messages.length === 0 && !historyHint && !restoring && loading && (
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
            return <UserMessageView key={idx} msg={msg} />;
          }

          const text = messageText(msg);
          const isError =
            text.includes('[错误:') ||
            text.includes('[连接错误:') ||
            text.includes('[Error:') ||
            text.includes('[Connection error:');
          const streaming = isLast && busy;
          // 终条且简短（无工具、单行）时，去掉多余的“时间线末端”橙点，只留一个起始点。
          const terse =
            isLast && !streaming && !messageHasTools(msg) && !text.includes('\n');
          const dotClass = isError ? 'dot-error' : 'dot-brand';
          const cls =
            'timeline-message ' +
            dotClass +
            (streaming ? ' dot-blink' : '') +
            (isLast ? ' is-last' : '') +
            (terse ? ' is-terse' : '');

          return (
            <div key={idx} class={cls}>
              {/* Error turns are pure injected text — render flat. */}
              {isError ? (
                <div class="error-message-content">
                  {text}
                  {streaming && <span class="streaming-cursor" />}
                </div>
              ) : (
                <>
                  {/* Segments in chronological order: text→tool→text→tool,
                      matching the TUI. Consecutive tools share one tool-list. */}
                  {renderAssistantParts(msg.parts)}
                  {streaming && <span class="streaming-cursor" />}
                </>
              )}
            </div>
          );
        })}

        {/* 排队中的消息：执行中输入、待当前回合结束后自动发送，可点 × 撤回。 */}
        {queued.map((q) => (
          <div key={`q-${q.id}`} class="user-message-wrapper queued">
            <div class="user-message-bubble">
              {q.images && q.images.length > 0 && (
                <div class="msg-images">
                  {q.images.map((img, i) => (
                    <img key={i} class="msg-image" src={imageDataUrl(img)} alt="" />
                  ))}
                </div>
              )}
              <div class="queued-head">
                <span class="queued-tag">{t('chat.queued')}</span>
                <button
                  class="queued-remove"
                  onClick={() => setQueued((arr) => arr.filter((x) => x.id !== q.id))}
                  title={t('chat.removeQueued')}
                  aria-label={t('chat.removeQueued')}
                >
                  ×
                </button>
              </div>
              {q.text}
            </div>
          </div>
        ))}

        <div ref={bottomRef} />
      </div>

      {/* Floating input */}
      <div class="input-container">{inputBox}</div>
      {filePickerModal}
      {livePermissionCard}
    </>
  );
}

/** Render an assistant message's ordered parts in chronological order: each
 *  text run becomes Markdown; runs of consecutive tool calls share one
 *  `.tool-list` container. This is what preserves the text→tool→text→tool
 *  interleaving (matching the TUI) instead of grouping all tools at the head. */
function renderAssistantParts(parts: MsgPart[]): VNode[] {
  const out: VNode[] = [];
  let i = 0;
  while (i < parts.length) {
    const p = parts[i];
    if (p.kind === 'tool') {
      const groupKey = i;
      const tools: ToolRow[] = [];
      while (i < parts.length) {
        const q = parts[i];
        if (q.kind !== 'tool') break;
        tools.push(q.tool);
        i++;
      }
      out.push(
        <div class="tool-list" key={`tg-${groupKey}`}>
          {tools.map((tool) => (
            <ToolRowView key={tool.id} tool={tool} />
          ))}
        </div>,
      );
    } else {
      if (p.text) out.push(<Markdown key={`tx-${i}`} content={p.text} />);
      i++;
    }
  }
  return out;
}

function UserMessageView({ msg }: { msg: Message }) {
  const t = useT();
  // 技能/文档型消息默认折叠为一行徽章，点击展开查看原文。
  const text = messageText(msg);
  const skillTitle = detectSkillContent(text);
  const [expanded, setExpanded] = useState(false);

  const images = msg.images && msg.images.length > 0 && (
    <div class="msg-images">
      {msg.images.map((img, i) => (
        <img key={i} class="msg-image" src={imageDataUrl(img)} alt="" />
      ))}
    </div>
  );

  if (skillTitle && !expanded) {
    return (
      <div class="user-message-wrapper">
        {images}
        <button
          class="skill-badge"
          onClick={() => setExpanded(true)}
          title={t('chat.skillExpand')}
        >
          <span class="skill-badge-icon" aria-hidden="true">⚡</span>
          <span class="skill-badge-label">{skillTitle}</span>
          <span class="skill-badge-hint">{t('chat.skillExpand')}</span>
        </button>
      </div>
    );
  }

  return (
    <div class="user-message-wrapper">
      <div class={'user-message-bubble' + (skillTitle ? ' is-markdown' : '')}>
        {images}
        {skillTitle && (
          <button class="skill-collapse" onClick={() => setExpanded(false)}>
            {t('chat.skillCollapse')}
          </button>
        )}
        {/* 技能/文档型内容本就是 markdown（注入的 SKILL.md），渲染它；
            普通用户消息保持逐字纯文本（不把用户输入当 markdown 解析）。 */}
        {skillTitle ? <Markdown content={text} /> : text}
      </div>
    </div>
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
    // 统一用状态词「完成」，不再显示耗时——否则实时执行时右侧是耗时（0.00s），
    // 刷新后历史快照不带 duration_ms 又变「完成」，两处不一致。
    annotation = { cls: 'success', label: t('tool.done') };
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
