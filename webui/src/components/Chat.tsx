// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App

import { VNode } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { streamChat, stopChat, SSEEvent, getSession, SessionMetaWithProject, getModels, ImageData, streamLive, postLiveMessage, postLiveStop, postLivePermission, postLiveProvider, postLiveSwitchSession, LiveWireEvent, SessionMessage, getSkills, SkillInfo, listDir } from '../api';
import { resolvePendingAfterDecision } from '../lib/pendingPermission';
import { Markdown } from './Markdown';
import { ModelSelector } from './ModelSelector';
import { AttachMenu } from './AttachMenu';
import { FilePicker } from './FilePicker';
import { PermissionCard } from './PermissionCard';
import { useT } from '../settings';
import {
  applyAtMentionSelection,
  detectAtMentionRange,
  ensureActiveDescendantVisible,
  splitAtToken,
} from '../lib/atMention';
import { upsertToolPart, type ToolRow, type MsgPart } from '../lib/toolRows';

interface Message {
  role: 'user' | 'assistant';
  parts: MsgPart[];
  images?: ImageData[];
}

/** Concatenate all text segments (error-detection, skill-title, etc.). */
function messageText(m: Message): string {
  return m.parts.reduce((acc, p) => (p.kind === 'text' ? acc + p.text : acc), '');
}

/** Format all parts of a message as readable text (including tool calls and their
 *  output), matching what is displayed on the page. Used by the copy button to
 *  copy the full visible content of an assistant turn. */
function messageFullText(m: Message): string {
  const lines: string[] = [];
  for (const p of m.parts) {
    if (p.kind === 'text') {
      lines.push(p.text);
    } else if (p.kind === 'tool') {
      const tool = p.tool;
      lines.push(`🔧 ${displayToolName(tool.name)}`);
      const detail = formatToolDetail(tool.name, tool.args);
      if (detail) lines.push(`   ${detail}`);
      if (tool.args) lines.push(`   参数: ${tool.args}`);
      if (tool.output) lines.push(`   输出: ${tool.output}`);
    } else if (p.kind === 'notice') {
      lines.push(p.text);
    } else if (p.kind === 'rate_limited') {
      lines.push(p.text);
    }
  }
  return lines.join('\n');
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
  /** /live turn 完成后通知 App 刷新侧栏列表（session 已落盘，列表需更新）。 */
  onLiveTurnDone?: () => void;
  /** 首条消息发出瞬间上报标题（取消息前 10 字），供 App 乐观插入侧栏，
   *  让会话即时出现；待后端落盘并自动命名后，列表刷新会换成真实标题。 */
  onOptimisticSession?: (title: string) => void;
  /** 打开工作目录选择器（cwd 面包屑已从顶栏移到输入框下方，由本组件渲染）。 */
  onOpenCwd?: () => void;
  /** 另一端（TUI /cd、worktree、其他 webui tab）切了工作目录：实时流送来 working_dir
   *  事件时上报新路径，供 App 更新 cwd 面包屑 + 侧栏目录过滤。 */
  onCwdChanged?: (dir: string) => void;
  /** AI 自动命名了当前会话：实时流送来 session_renamed 事件时上报新名称，
   *  供 App 更新顶部标题头，无需再发请求拉取会话列表。 */
  onSessionRenamed?: (name: string) => void;
  /** 上报是否处于落地（空对话）态，供 App 决定是否显示会话标题头。 */
  onLanding?: (landing: boolean) => void;
  /** 侧栏「技能」菜单选中的技能：变化时把 `/name ` 插入输入框。 */
  skillInsert?: { name: string; seq: number } | null;
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

// Mirror of the TUI's `display_tool_name` (event_loop/mod.rs): MCP wire names
// `mcp__server__tool` render as `mcp · server · tool`; everything else is
// snake_case → PascalCase (`read_file` → `ReadFile`). Keeps the webui's tool
// headers identical to the terminal instead of showing raw `mcp__…` names.
function displayToolName(name: string): string {
  if (name.startsWith('mcp__')) {
    const rest = name.slice('mcp__'.length);
    const i = rest.indexOf('__');
    if (i >= 0) return `mcp · ${rest.slice(0, i)} · ${rest.slice(i + 2)}`;
  }
  return name
    .split('_')
    .filter((w) => w.length > 0)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join('');
}

// Mirror of the TUI's `format_tool_detail`: a compact human-readable summary
// of a call's arguments (e.g. MCP calls as `key: "value", …` instead of raw
// JSON). `argsJson` is the stored arguments string; the full raw args stay
// available by expanding the row. Returns '' when there's nothing useful to
// show (the header then shows just the name, like the TUI).
function formatToolDetail(name: string, argsJson: string): string {
  let v: Record<string, unknown>;
  try {
    const parsed = JSON.parse(argsJson);
    if (parsed === null || typeof parsed !== 'object') return '';
    v = parsed as Record<string, unknown>;
  } catch {
    return argsJson; // not JSON — show as-is rather than nothing
  }
  const getStr = (k: string): string => (typeof v[k] === 'string' ? (v[k] as string) : '');
  const basename = (p: string) => p.split('/').pop() || p;

  switch (name) {
    case 'read_file':
    case 'edit_file':
    case 'write_file':
    case 'create_file':
    case 'list_symbols':
      return getStr('file_path') ? basename(getStr('file_path')) : '';
    case 'read_symbol': {
      const sym = getStr('symbol');
      const file = getStr('file_path') ? basename(getStr('file_path')) : '';
      if (!sym) return file;
      if (!file) return sym;
      return `${sym} in ${file}`;
    }
    case 'glob':
    case 'grep':
      return getStr('pattern');
    case 'bash':
      return getStr('command');
    case 'list_directory':
    case 'change_dir':
      return getStr('path') || '.';
    case 'web_fetch':
      return getStr('url');
    case 'web_search':
      return getStr('query');
    case 'find_references':
    case 'trace_callees':
    case 'trace_callers':
    case 'trace_chain':
      return getStr('symbol');
    case 'blast_radius':
    case 'file_dependencies':
      return getStr('file') ? basename(getStr('file')) : '';
    case 'search_replace': {
      const s = getStr('search');
      const r = getStr('replace');
      if (s && r) {
        const parts = [`${s} → ${r}`];
        const glob = getStr('glob');
        const path = getStr('path');
        if (glob) parts.push(`glob: ${glob}`);
        if (path && path !== '.') parts.push(`path: ${basename(path)}`);
        return parts.join(', ');
      }
      return r || s || '';
    }
    case 'parallel_edit_files': {
      const files = Array.isArray(v.files) ? (v.files as unknown[]) : null;
      if (!files) return '';
      return files
        .map((e) => {
          const p = (e as Record<string, unknown>)?.path;
          return typeof p === 'string' ? basename(p) : null;
        })
        .filter((x): x is string => x !== null)
        .join(', ');
    }
    case 'todo': {
      const action = getStr('action');
      if (action === 'add') return getStr('content');
      if (action === 'update') {
        const id = typeof v.id === 'number' ? v.id : '';
        const status = getStr('status');
        if (id && status) return `#${id} → ${status}`;
        if (id) return `#${id}`;
        return status;
      }
      if (action === 'list') return 'list all';
      return '';
    }
    case 'use_skill':
      return getStr('name');
    default: {
      // MCP tools (`mcp__server__tool`): render args as `key: "value"` pairs.
      if (name.startsWith('mcp__')) {
        const pairs: string[] = [];
        for (const [k, val] of Object.entries(v)) {
          let s: string;
          if (typeof val === 'string') s = val;
          else if (typeof val === 'number' || typeof val === 'boolean') s = String(val);
          else if (val && typeof val === 'object') s = JSON.stringify(val);
          else continue;
          if (!s) continue;
          pairs.push(`${k}: "${s.replace(/"/g, '\\"')}"`);
        }
        if (pairs.length) return pairs.join(', ');
      }
      // Fallback: first present common single-key arg.
      for (const key of ['file_path', 'path', 'file', 'pattern', 'query', 'url', 'name', 'symbol', 'command']) {
        const s = getStr(key);
        if (s) return s;
      }
      return '';
    }
  }
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

export function Chat({ sessionId, onSessionId, cwd, onPermission, onPermissionResolved, activeSession, restoring, onLiveTurnDone, onOptimisticSession, onOpenCwd, onCwdChanged, onLanding, skillInsert, onSessionRenamed }: ChatProps) {
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
  const requestIdRef = useRef<string | null>(null);
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
  // 是否已为「当前会话」上报过乐观侧栏条目。每次切换/新建会话时复位，
  // 避免同一会话第二条消息（尤其 sync 路径本地不落消息）重复上报、改写标题。
  const optimisticFiredRef = useRef(false);

  // 切换/恢复会话时重置画布并加载历史。依赖 project_hash：刷新后 sessionId 先于
  // 元数据就绪，此时只显示提示；待 App 从会话列表回填 project_hash，本 effect 因
  // 依赖变化重跑，再真正拉取历史。
  useEffect(() => {
    // 会话 id 变化（外部切换 / 新建按钮）才重置画布。本 Chat 自建会话首条消息完成后
    // sessionId 变成自己的 id（activeIdRef 已同步），不重置，以免清空刚看到的对话。
    if (sessionId !== activeIdRef.current) {
      activeIdRef.current = sessionId;
      loadedForRef.current = null;
      optimisticFiredRef.current = false;
      abortRef.current?.abort();
      setBusy(false);
      setMessages([]);
      setTokens(null);
      setHistoryHint(null);
      // 切到一个有 id 的会话 → 进入「加载中」，先抑制落地页（避免闪屏）；
      // 无 id（新建）则不加载、直接落地。
      setLoading(sessionId != null);
      // sync 模式：本端在侧栏切到另一个（已存在）会话时，通知后端广播会话切换，
      // 使同进程 sync 模式的 TUI 跟随加载该会话历史。
      // 不会回环：远端 session_switched 事件的 handler 会先把 activeIdRef 设为该 id，
      // 故由广播回流引起的 sessionId 变化进不来这个分支（条件已不成立），不会再次广播。
      if (sync && sessionId) {
        postLiveSwitchSession(sessionId).catch(() => {});
      }
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
  const { scopeDir: atDirPart, filter: atFilter } = splitAtToken(atQuery);
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

  useEffect(() => {
    if (!atOpen) return;
    requestAnimationFrame(() => {
      const container = atRef.current;
      const active = container?.querySelector<HTMLButtonElement>('.at-row.active');
      if (container && active) ensureActiveDescendantVisible(container, active);
    });
  }, [atIndex, atOpen, atRows.length]);

  // ── 共享的实时流启/停逻辑 ──
  function startLiveStream() {
    // Abort any prior stream FIRST. /live is a broadcast channel, so a leaked
    // subscription would re-deliver every turn event (duplicate tool rows, double
    // token counts). startLiveStream is reachable from mount, toggleSync, and
    // session_switched — without this, those overlap into N concurrent streams.
    liveAbortRef.current?.abort();
    const controller = new AbortController();
    liveAbortRef.current = controller;
    streamLive(onLiveEvent, controller.signal, activeIdRef.current).catch(() => {
      // Stream ended or errored; turn sync back off — but NOT when the
      // stream was deliberately aborted (session switch / manual toggle),
      // because a new stream is already being (re)started and setting
      // sync=false here would cause the next user message to go through
      // /chat instead of /live/message, breaking TUI sync output.
      if (controller.signal.aborted) return;
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

  // 把 sync 状态写回 URL 的 ?sync 参数，使刷新后能保持当前开/关状态
  // （否则关掉同步后 URL 仍带 sync=1，刷新一下又被重新开启 —— issue #816）。
  // 覆盖所有改变 sync 的入口：toggleSync、以及实时流出错时的自动关闭。
  // 用 replaceState 避免在浏览器历史里堆积条目。
  useEffect(() => {
    try {
      const url = new URL(location.href);
      if (sync) url.searchParams.set('sync', '1');
      else url.searchParams.delete('sync');
      history.replaceState(history.state, '', url.toString());
    } catch { /* URL/history 不可用时忽略 */ }
  }, [sync]);

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
      case 'warning': return { type: 'warning', message: e.message };
      case 'rate_limited': return { type: 'rate_limited', reset_at_display: e.reset_at_display, reset_label: e.reset_label, secs_until_reset: e.secs_until_reset, auto_resuming: e.auto_resuming };
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
    // 工作目录切换是进程级（另一端 /cd），与查看哪个会话无关 → 不门控，始终上报
    // 让 App 更新 cwd 面包屑 + 侧栏目录过滤。会话本身不变（对话保留）。
    if (e.type === 'working_dir') {
      onCwdChanged?.(e.working_dir);
      return;
    }
    // AI 自动命名了某个会话：通知 App 更新标题头，无需拉取列表。
    // 实时流是进程级广播，会到达所有 tab；仅当被改名的正是本 tab 正在
    // 查看的会话时才应用，否则会把别的会话的名字盖到当前标题头上。
    if (e.type === 'session_renamed') {
      if (e.session_id === liveSessionIdRef.current) {
        onSessionRenamed?.(e.name);
      }
      return;
    }
    // 会话切换：另一端（webui 新建对话 / TUI /session）创建了新会话，
    // 本端跟随切换——更新 session id 并重置画布。
    // 不设置 loadedForRef：让 Chat useEffect 在 activeSession 到位后
    // 正常走 getSession 加载（空）历史并清除 historyHint；
    // 否则 loadedForRef 会阻止加载，导致 historyHint 永远不被清除。
    // 重启 SSE 连接：旧连接订阅的是旧 LiveSession，后续 turn 事件不会到达；
    // 重新连接后 /live handler 会绑定到新 LiveSession。
    if (e.type === 'session_switched') {
      liveSessionIdRef.current = e.session_id;
      // 本端正是发起此次切换的视图（侧栏切到已存在会话→postLiveSwitchSession→广播
      // 回流），或重复广播：此时我们已经在该会话上、历史也正在/已经加载，绝不能再清空
      // 画布，否则刚 getSession 加载好的历史会被自己的广播回流抹掉（且 sessionId 未变，
      // 加载 effect 不会重跑，history 永远回不来）。仅把实时流重绑到新 LiveSession 即可。
      const alreadyViewing = activeIdRef.current === e.session_id;
      activeIdRef.current = e.session_id;
      if (!alreadyViewing) {
        onSessionId(e.session_id);
        setMessages([]);
        setTokens(null);
        setHistoryHint(null);
      }
      if (sync) {
        stopLiveStream();
        startLiveStream();
      }
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
        if (!e.running) {
          setLivePending(null);
          // turn 完成后 session 已落盘，通知 App 刷新侧栏列表。
          onLiveTurnDone?.();
        }
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
        // 重新开启同步：先把当前会话广播给 TUI，再启动本端实时流。
        // 关闭同步期间 webui 可能已切换/新建到别的会话（不广播），此时全局 LiveSession
        // 仍绑在旧会话、TUI 也订阅着旧实例。若直接 startLiveStream，/live 会以新 session_id
        // 替换全局 LiveSession 却不通知旧实例的订阅者，TUI 就此被孤立、停在旧会话不跟随。
        // 故必须先 postLiveSwitchSession：它在「旧」全局实例上广播 SessionSwitched，TUI 据此
        // 加载本端会话并把实时总线重绑到新实例——与侧栏切换会话的同步路径一致。
        // 顺序关键：必须等广播发出（落在 TUI 仍订阅的旧实例上）后再启动会替换全局的实时流。
        const sid = activeIdRef.current;
        if (sid) {
          postLiveSwitchSession(sid).catch(() => {}).finally(() => startLiveStream());
        } else {
          startLiveStream();
        }
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

  // Append a non-fatal advisory as its OWN notice part (never merged into a text run,
  // never styled as an error). Mirrors appendToLastAssistant's last-assistant guard.
  function pushNoticeToLastAssistant(text: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      const parts: MsgPart[] = [...last.parts, { kind: 'notice', text }];
      return [...prev.slice(0, -1), { ...last, parts }];
    });
  }

  function pushRateLimitedToLastAssistant(text: string) {
    setMessages((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      if (last.role !== 'assistant') return prev;
      const parts: MsgPart[] = [...last.parts, { kind: 'rate_limited', text }];
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
      // Dedup by call_id: a tool_start re-delivered (e.g. a leaked /live
      // subscription replays the turn) must update the existing row, not append
      // a duplicate. See lib/toolRows.upsertToolPart.
      return [...prev.slice(0, -1), { ...last, parts: upsertToolPart(last.parts, tool) }];
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

      case 'warning':
        // 非致命提示（如"已自动压缩上下文"）：渲染成淡色 notice 行 —— 不染红、不并进
        // 回复文本、不结束回合（任务继续）。对齐 TUI 的黄色 "!" 提示。
        pushNoticeToLastAssistant(t('chat.warning', { msg: event.message }));
        break;

      case 'rate_limited': {
        // 限流暂停：渲染成暗色中性卡片，非红色 error 样式；保留已完成内容，不结束回合。
        // auto_resuming=true → WaitAndRetry (kernel will sleep then retry)
        // auto_resuming=false + reset_at_display → Pause with known reset time
        // auto_resuming=false + empty reset_at_display → Pause with unknown reset time
        const time = event.reset_at_display;
        let text: string;
        if (event.auto_resuming) {
          text = t('chat.rateLimited.waiting', { secs: String(event.secs_until_reset ?? 0) });
        } else if (time) {
          text = `${t('chat.rateLimited.paused', { time })} · ${t('chat.rateLimited.hint')}`;
        } else {
          // Pause with no wall-clock time but a known remaining duration: show it
          // (compact h/m/s, matching the TUI) instead of dropping secs_until_reset.
          const secs = event.secs_until_reset;
          const dur = secs == null ? '' :
            secs >= 3600 ? `（还有 ${Math.floor(secs / 3600)}h${Math.floor((secs % 3600) / 60)}m）` :
            secs >= 60 ? `（还有 ${Math.floor(secs / 60)}m）` : `（还有 ${secs}s）`;
          text = `${t('chat.rateLimited.pausedNoTime')}${dur} · ${t('chat.rateLimited.hint')}`;
        }
        pushRateLimitedToLastAssistant(text);
        break;
      }

      default:
        // Ignore tool_batch, artifact_*, etc.
        break;
    }
  }

  // 实际投递一条消息（同步 / 常规两条路径）；busy 由各自的事件流复位。
  async function deliver(text: string, images: ImageData[]) {
    // 本会话首条消息：用消息前 10 字做临时标题，立刻通知 App 乐观插入侧栏，
    // 让会话「一发送就出现在左侧」。回合 done 后列表刷新会换成后端自动命名。
    if (!optimisticFiredRef.current && messages.length === 0) {
      optimisticFiredRef.current = true;
      const title = (text.split('\n')[0]?.trim() ?? '').slice(0, 10);
      if (title) onOptimisticSession?.(title);
    }

    if (sync) {
      // ── Sync path: send to /live/message; do NOT locally append (the user
      //    event will arrive back via the live stream, keeping all tabs in sync).
      setBusy(true);
      await postLiveMessage(text, images.length ? images : undefined, provider ?? undefined, activeIdRef.current);
      // 消息发出后延迟刷新侧栏列表，给后端落盘时间；
      // turn 完成后 state(running=false) 会再刷一次确保更新。
      setTimeout(() => onLiveTurnDone?.(), 200);
      return;
    }

    // ── Normal path ──
    setBusy(true);
    // 消息发出后延迟刷新侧栏列表，给后端落盘时间；
    // done 事件中 onSessionId 会再刷一次确保更新。
    setTimeout(() => onLiveTurnDone?.(), 200);

    // Push user message + empty assistant placeholder
    setMessages((prev) => [
      ...prev,
      { role: 'user', parts: [{ kind: 'text', text }], images: images.length ? images : undefined },
      { role: 'assistant', parts: [] },
    ]);

    const controller = new AbortController();
    abortRef.current = controller;
    const requestId = crypto.randomUUID();
    requestIdRef.current = requestId;

    try {
      const body = {
        message: text,
        ...(sessionId ? { session_id: sessionId } : {}),
        request_id: requestId,
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
      if (requestIdRef.current === requestId) requestIdRef.current = null;
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

  async function handleStop() {
    try {
      if (sync) {
        await postLiveStop();
      } else if (requestIdRef.current) {
        await stopChat(requestIdRef.current);
      }
    } catch {
      // If the cancellation endpoint itself is unavailable, at least restore
      // the local UI instead of leaving the stop button stuck indefinitely.
      abortRef.current?.abort();
      setBusy(false);
      setQueued([]);
      onPermissionResolved?.(null);
    }
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

  // 把当前 @ 段替换为相对路径 rel，不自动追加空格，目录可继续下钻补全。
  function setAtMention(rel: string, isDir: boolean) {
    const ta = textareaRef.current;
    if (!ta) return;
    const pos = ta.selectionStart ?? ta.value.length;
    const range = detectAtMentionRange(ta.value, pos);
    if (!range) return;
    const next = applyAtMentionSelection(ta.value, range, rel, isDir);
    setInput(next.text);
    setAtOpen(next.keepOpen);
    setAtQuery(next.query);
    if (next.keepOpen) setAtItems([]);
    setAtIndex(0);
    requestAnimationFrame(() => {
      ta.focus();
      ta.setSelectionRange(next.cursor, next.cursor);
      ta.style.height = 'auto';
      ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
    });
  }

  // 选择 @ 菜单某一行：「..」仍用于返回上级；目录/文件都插入完整相对路径。
  function chooseAtRow(row: { name: string; is_dir: boolean; up?: boolean }) {
    if (row.up) {
      const trimmed = atDirPart.replace(/\/+$/, '');
      const idx = trimmed.lastIndexOf('/');
      const parent = idx >= 0 ? trimmed.slice(0, idx + 1) : '';
      const ta = textareaRef.current;
      if (!ta) return;
      const range = detectAtMentionRange(ta.value, ta.selectionStart ?? ta.value.length);
      if (!range) return;
      const nextText = ta.value.slice(0, range.start) + `@${parent}` + ta.value.slice(range.end);
      setInput(nextText);
      setAtQuery(parent);
      setAtIndex(0);
      requestAnimationFrame(() => {
        ta.focus();
        const cursor = range.start + 1 + parent.length;
        ta.setSelectionRange(cursor, cursor);
      });
    } else {
      setAtMention(atDirPart + row.name + (row.is_dir ? '/' : ''), row.is_dir);
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
    const atRange = detectAtMentionRange(val, pos);
    if (atRange) {
      const query = atRange.token;
      if (query.length <= 120) {
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
  // 若 `replaceSkill` 为 true，先清空输入框再插入，避免反复选择技能时累加。
  function insertAtCursor(text: string, replaceSkill = false) {
    const ta = textareaRef.current;
    if (replaceSkill) {
      setInput(text);
    } else if (!ta) {
      setInput((v) => v + text);
    } else {
      const start = ta.selectionStart ?? ta.value.length;
      const end = ta.selectionEnd ?? ta.value.length;
      setInput(ta.value.slice(0, start) + text + ta.value.slice(end));
    }
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      const pos = replaceSkill ? text.length : (ta?.selectionStart ?? el.value.length) + text.length;
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

  // 上报落地态给 App（决定是否显示会话标题头）。
  useEffect(() => {
    onLanding?.(landing);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [landing]);

  // 侧栏「技能」菜单选中 → 把 `/name ` 插入输入框（按 seq 去重，避免重复插入）。
  // replaceSkill=true 会先清除已有的技能前缀，避免反复选择时累加。
  const lastSkillSeqRef = useRef<number | null>(null);
  useEffect(() => {
    if (!skillInsert) return;
    if (lastSkillSeqRef.current === skillInsert.seq) return;
    lastSkillSeqRef.current = skillInsert.seq;
    insertAtCursor(`/${skillInsert.name} `, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skillInsert]);

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
              title={item.up ? '..' : atDirPart + item.name + (item.is_dir ? '/' : '')}
            >
              <span class="at-icon">{item.up ? '⬆' : item.is_dir ? '📁' : '📄'}</span>
              <span class="at-name">{item.up ? '..' : atDirPart + item.name + (item.is_dir ? '/' : '')}</span>
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
          {/* lucide `arrow-left-right` — matches the pencil design's sync icon. */}
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M8 3 4 7l4 4" />
            <path d="M4 7h16" />
            <path d="m16 21 4-4-4-4" />
            <path d="M20 17H4" />
          </svg>
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

  // 输入框下方副栏：左 cwd 面包屑（点击切目录），右键盘提示（对齐设计的 Input Footer）。
  const inputSubbar = (
    <div class="input-subbar">
      <button class="input-cwd" onClick={() => onOpenCwd?.()} title={t('header.switchCwd')}>
        <svg class="input-cwd-icon" width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M1.8 4.2c0-.66.54-1.2 1.2-1.2h2.7l1.3 1.5h5.2c.66 0 1.2.54 1.2 1.2v5.9c0 .66-.54 1.2-1.2 1.2H3c-.66 0-1.2-.54-1.2-1.2z"
            stroke="currentColor"
            stroke-width="1.2"
            stroke-linejoin="round"
          />
        </svg>
        {cwd ? (
          <span class="input-cwd-path">{projPath}</span>
        ) : (
          <span class="input-cwd-path muted">{t('header.noCwd')}</span>
        )}
        <span class="input-cwd-chevron">▾</span>
      </button>
      <span class="input-hint">{t('chat.kbdHint')}</span>
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
      onDecide={async (decision, toolName) => { await postLivePermission(decision, toolName); }}
    />
  );

  // 落地页快捷提示胶囊：点击把文本填入输入框并聚焦（不自动发送，便于二次编辑）。
  const quickChips: { label: string; insert: string }[] = [
    { label: t('chat.chipReview'), insert: '/code-review ' },
    { label: t('chat.chipExplain'), insert: t('chat.chipExplain') },
    { label: t('chat.chipTest'), insert: t('chat.chipTest') },
  ];
  function fillInput(text: string) {
    setInput(text);
    requestAnimationFrame(() => {
      const ta = textareaRef.current;
      if (!ta) return;
      ta.focus();
      const pos = text.length;
      ta.setSelectionRange(pos, pos);
      ta.style.height = 'auto';
      ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
    });
  }

  if (landing) {
    return (
      <>
        <div class="chat-landing">
          <div class="landing-inner">
            <div class="landing-brand">
              {/* <span class="landing-brand-logo" aria-hidden="true">
                <svg width="34" height="34" viewBox="0 0 24 24" fill="none">
                  <rect x="6.4" y="6.4" width="11.2" height="11.2" rx="2.6" transform="rotate(45 12 12)" stroke="currentColor" stroke-width="1.8" />
                </svg>
              </span> */}
              <span class="landing-brand-name">AtomCode</span>
            </div>
            <div class="landing-tagline">{t('chat.greeting')}</div>
            <div class="landing-input">
              {inputBox}
              {inputSubbar}
            </div>
            <div class="landing-chips">
              {quickChips.map((c) => (
                <button key={c.label} class="landing-chip" onClick={() => fillInput(c.insert)}>
                  {c.label}
                </button>
              ))}
            </div>
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
        <div class="timeline-inner">
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

        {(() => {
          // Pre-compute per-turn text for assistant messages: collect all text
          // from consecutive assistant messages between two user messages, so
          // the copy button can copy the entire LLM turn (not just one chunk).
          const turnTexts = new Map<number, string>();
          {
            let start = -1;
            let parts: string[] = [];
            for (let i = 0; i < messages.length; i++) {
              if (messages[i].role === 'assistant') {
                if (start < 0) start = i;
                const t = messageFullText(messages[i]);
                if (t) parts.push(t);
              } else {
                if (start >= 0) {
                  for (let j = start; j < i; j++) {
                    turnTexts.set(j, parts.join('\n\n'));
                  }
                  start = -1;
                  parts = [];
                }
              }
            }
            // Flush the last turn
            if (start >= 0) {
              for (let j = start; j < messages.length; j++) {
                turnTexts.set(j, parts.join('\n\n'));
              }
            }
          }

          return messages.map((msg, idx) => {
            const isLast = idx === lastIdx;
            if (msg.role === 'user') {
              return <UserMessageView key={idx} msg={msg} />;
            }

            // Determine if this assistant message is the last one in the current
            // turn (i.e. the next message is a user message, or this is the very
            // last message in the list). Only the last assistant message in a turn
            // gets the copy button, so one user turn → one copy button.
            const isLastInTurn =
              idx === lastIdx ||
              messages[idx + 1]?.role === 'user';

            return (
              <AssistantMessageView
                key={idx}
                msg={msg}
                isLast={isLast}
                busy={busy}
                lastIdx={lastIdx}
                isLastInTurn={isLastInTurn}
                turnText={turnTexts.get(idx) ?? ''}
              />
            );
          });
        })()}

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
      </div>

      {/* Floating input */}
      <div class="input-container">
        <div class="input-wrap">
          {inputBox}
          {inputSubbar}
        </div>
      </div>
      {filePickerModal}
      {livePermissionCard}
    </>
  );
}

/** Render an assistant message's ordered parts in chronological order: each
 *  text run becomes Markdown; runs of consecutive tool calls share one
 *  `.tool-list` container. This is what preserves the text→tool→text→tool
 *  interleaving (matching the TUI) instead of grouping all tools at the head. */
function AssistantMessageView({ msg, isLast, busy, isLastInTurn, turnText }: { msg: Message; isLast: boolean; busy: boolean; lastIdx: number; isLastInTurn: boolean; turnText: string }) {
  const t = useT();
  const text = messageText(msg);
  const isError =
    text.includes('[错误:') ||
    text.includes('[连接错误:') ||
    text.includes('[Error:') ||
    text.includes('[Connection error:');
  const streaming = isLast && busy;
  // 终条且简短（无工具、单行）时，去掉多余的"时间线末端"橙点，只留一个起始点。
  const terse =
    isLast && !streaming && !messageHasTools(msg) && !text.includes('\n');
  const dotClass = isError ? 'dot-error' : 'dot-brand';
  const cls =
    'timeline-message ' +
    dotClass +
    (streaming ? ' dot-blink' : '') +
    (isLast ? ' is-last' : '') +
    (terse ? ' is-terse' : '');

  // Copy button: only shown on the last assistant message in a turn.
  // Copies the entire turn's text (all assistant messages in this turn joined).
  const [copied, setCopied] = useState(false);

  function handleCopy() {
    navigator.clipboard.writeText(turnText).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }).catch(() => {});
  }

  const copyBtn = isLastInTurn && !isError && !streaming && turnText ? (
    <div class="msg-actions msg-actions-left">
      <button
        class={'msg-copy-btn' + (copied ? ' copied' : '')}
        onClick={handleCopy}
        title={copied ? t('copy.copied') : t('copy.copy')}
        aria-label={copied ? t('copy.copied') : t('copy.copy')}
      >
        {copied ? (
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M3.5 8.5 6.5 11.5 12.5 4.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        ) : (
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <rect x="5" y="5" width="8.5" height="8.5" rx="1.5" stroke="currentColor" stroke-width="1.2" />
            <path d="M2.5 10.5V3.5A1.5 1.5 0 0 1 4 2h7" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
          </svg>
        )}
      </button>
    </div>
  ) : null;

  return (
    <div class={cls}>
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
      {copyBtn}
    </div>
  );
}

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
    } else if (p.kind === 'notice') {
      out.push(
        <div class="msg-notice" key={`nt-${i}`}>
          {p.text}
        </div>,
      );
      i++;
    } else if (p.kind === 'rate_limited') {
      out.push(
        <div class="rate-limited-notice" key={`rl-${i}`}>
          {p.text}
        </div>,
      );
      i++;
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
  const [copied, setCopied] = useState(false);

  function handleCopy() {
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }).catch(() => {});
  }

  const images = msg.images && msg.images.length > 0 && (
    <div class="msg-images">
      {msg.images.map((img, i) => (
        <img key={i} class="msg-image" src={imageDataUrl(img)} alt="" />
      ))}
    </div>
  );

  const copyBtn = (
    <button
      class={'msg-copy-btn' + (copied ? ' copied' : '')}
      onClick={handleCopy}
      title={copied ? t('copy.copied') : t('copy.copy')}
      aria-label={copied ? t('copy.copied') : t('copy.copy')}
    >
      {copied ? (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path d="M3.5 8.5 6.5 11.5 12.5 4.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect x="5" y="5" width="8.5" height="8.5" rx="1.5" stroke="currentColor" stroke-width="1.2" />
          <path d="M2.5 10.5V3.5A1.5 1.5 0 0 1 4 2h7" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
        </svg>
      )}
    </button>
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
      <div class="msg-actions">
        {copyBtn}
      </div>
    </div>
  );
}

// Map a tool's wire name to a leading line-icon category (mirrors the inline
// design: file / search / edit / terminal / globe / folder …).
function toolCategory(name: string): string {
  if (name.startsWith('mcp__')) return 'mcp';
  switch (name) {
    case 'read_file':
    case 'read_symbol':
    case 'list_symbols':
    case 'file_dependencies':
      return 'file';
    case 'edit_file':
    case 'write_file':
    case 'create_file':
    case 'search_replace':
    case 'parallel_edit_files':
      return 'edit';
    case 'grep':
    case 'glob':
    case 'find_references':
    case 'trace_callees':
    case 'trace_callers':
    case 'trace_chain':
    case 'blast_radius':
      return 'search';
    case 'bash':
      return 'terminal';
    case 'web_fetch':
    case 'web_search':
      return 'globe';
    case 'list_directory':
    case 'change_dir':
      return 'folder';
    case 'use_skill':
      return 'skill';
    case 'todo':
      return 'todo';
    default:
      return 'default';
  }
}

const TOOL_ICON_PATHS: Record<string, VNode> = {
  file: (
    <>
      <path d="M9 1.75H4.5A1.5 1.5 0 0 0 3 3.25v9.5a1.5 1.5 0 0 0 1.5 1.5h7a1.5 1.5 0 0 0 1.5-1.5V5.75L9 1.75Z" />
      <path d="M9 1.75v4h4" />
    </>
  ),
  edit: <path d="M11.4 2.6l2 2L6 12l-2.6.6L4 10l7.4-7.4Z" />,
  search: (
    <>
      <circle cx="7" cy="7" r="4.25" />
      <path d="M10.2 10.2 14 14" />
    </>
  ),
  terminal: (
    <>
      <rect x="2" y="3" width="12" height="10" rx="1.5" />
      <path d="M4.5 6.5 7 8.5 4.5 10.5" />
      <path d="M8.5 10.5h3" />
    </>
  ),
  globe: (
    <>
      <circle cx="8" cy="8" r="6" />
      <path d="M2 8h12" />
      <path d="M8 2c2.2 2.2 2.2 9.8 0 12-2.2-2.2-2.2-9.8 0-12Z" />
    </>
  ),
  folder: (
    <path d="M2 4.5A1.5 1.5 0 0 1 3.5 3h2.5l1.3 1.6h5.2A1.5 1.5 0 0 1 14 6.1v5.9a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 12V4.5Z" />
  ),
  skill: <path d="M9 1.5 3.5 9H7.5L7 14.5 12.5 7H8.5L9 1.5Z" />,
  todo: (
    <>
      <path d="M6.5 4.5H13M6.5 8H13M6.5 11.5H13" />
      <path d="M2.5 4.4l.8.8 1.5-1.7" />
      <circle cx="3.3" cy="8" r="0.5" fill="currentColor" stroke="none" />
      <circle cx="3.3" cy="11.5" r="0.5" fill="currentColor" stroke="none" />
    </>
  ),
  mcp: (
    <>
      <rect x="3" y="3" width="4.5" height="4.5" rx="1" />
      <rect x="8.5" y="8.5" width="4.5" height="4.5" rx="1" />
      <path d="M7.5 5.25h2A1.5 1.5 0 0 1 11 6.75v1.75" />
    </>
  ),
  default: <circle cx="8" cy="8" r="2.5" />,
};

function ToolTypeIcon({ name }: { name: string }) {
  return (
    <svg
      class="tool-type-icon"
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      stroke-width="1.4"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      {TOOL_ICON_PATHS[toolCategory(name)] ?? TOOL_ICON_PATHS.default}
    </svg>
  );
}

// Status glyph: a check when done, a cross on error, otherwise a spinner that
// animates while the call is pending / awaiting approval.
function ToolStatusIcon({ cls }: { cls: string }) {
  const spin = cls === 'pending' || cls === 'waiting';
  return (
    <svg
      class={'tool-status-icon' + (spin ? ' spin' : '')}
      width="12"
      height="12"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      {cls === 'success' ? (
        <path d="M3.5 8.5 6.5 11.5 12.5 4.5" />
      ) : cls === 'error' ? (
        <path d="M4 4l8 8M12 4l-8 8" />
      ) : (
        <path d="M8 2.5a5.5 5.5 0 1 1-5.18 3.65" />
      )}
    </svg>
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
        <ToolTypeIcon name={tool.name} />
        <span class="tool-name">{displayToolName(tool.name)}</span>
        <span class="tool-name-secondary">{abbreviateArgs(formatToolDetail(tool.name, tool.args))}</span>
        {annotation && (
          <span class={'tool-annotation ' + annotation.cls}>
            <ToolStatusIcon cls={annotation.cls} />
            {annotation.label}
          </span>
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
