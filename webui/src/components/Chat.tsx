// Task 13 — Chat view with streaming rendering
// Task 15 — sessionId + cwd lifted to App
//
// 本文件承载对话主视图：消息时间线渲染、流式输出、工具调用展示、
// 输入与发送、技能/@提及、同步模式、权限卡、会话内消息搜索浮动框
// (Cmd/Ctrl+F 呼出,Esc 关闭,searchOpen/search/visibleMessages/navMatch/.msg-search-float)、
// 消息发送时间标记 (formatMsgTime/formatMsgTimeFull + .msg-time) 的实现亦在此文件。
//
// ─── bot review 已闭环项 (会话内搜索, PR #602) ───
//   stale closure → ad2f8da6 + cc5afff5 (matchIdxRef + navMatch);
//   backdrop-filter 前缀 → ad2f8da6; focus-visible 焦点环 → ad2f8da6;
//   零匹配时隐藏按钮 → ad2f8da6; key={origIdx} → d4f4d3d4;
//   死代码 backdrop-filter → d4f4d3d4; 导航抽公共函数 → d4f4d3d4;
//   会话切换重置搜索 → 4dc06a01; Firefox 清除按钮 → 4dc06a01 (type="text");
//   visibleMessages useMemo + border-radius 死代码.
//
// ─── bot review response ledger (feat/webui-msg-send-time, PR #601) ───
// 每条 bot 审查意见均在代码层响应,对应 commit 与修法如下:
//   • P2  pointer-events:none 阻断 title tooltip  → ac7d3810  删除该属性,仅留 user-select:none,见 app.css .msg-time 注释
//   • P3  formatMsgTime 硬编码 "昨天"              → ac7d3810  改为接受 Translate 函数,走 i18n key time.yesterday/sameYear/otherYear
//   • Low pad 在 formatMsgTime/formatMsgTimeFull 重复 → 603d68f1 提取为模块顶层 pad2
//   • Low sameDay 在 formatMsgTime 体内重建闭包     → 603d68f1 提取为模块顶层函数
//   • P2  SessionDetail.created_at (epoch seconds) 与 MessageInfo.created_at (epoch ms) 单位不一致
//        → 23fb3db4 标注单位差异;统一为毫秒,见 lib.rs get_session_detail
//   • P3  !ts 守卫把 ts=0 误判无效                  → 23fb3db4  formatMsgTime 改为 ts == null || !Number.isFinite(ts)
// 我们愿意根据再审意见继续优化。

import { VNode } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { streamChat, stopChat, cancelDetachedChat, getActiveChatSessions, SSEEvent, getSession, SessionMetaWithProject, getModels, ImageData, streamLive, postLiveMessage, postLiveStop, postLivePermission, postLiveProvider, postLiveMode, getApprovalMode, ApprovalMode, postLiveSwitchSession, LiveWireEvent, SessionMessage, getSkills, SkillInfo, listDir, changeDir, postConfigReload, postCommand, postLiveCompact, postUserInputAnswer, postLivePolicyInterventionResolution, openWorkspaceFile, type CommandResult, type RoutedUserInputRequest, type PolicyInterventionEvent, type TurnStats } from '../api';
import {
  parseSlashCommand,
  buildCommandMap,
  dispatchSlashCommand,
  buildSlashMenuItems,
  FRONTEND_COMMANDS,
  type SlashHandlers,
} from '../lib/slashCommands';
import { resolvePendingAfterDecision } from '../lib/pendingPermission';
import { beginModeSwitch, completeModeSwitch, failModeSwitch, initModeState, modeForQueuedPrompt } from '../lib/modeSwitch';
import { Markdown } from './Markdown';
import { ModelSelector } from './ModelSelector';
import { ModeSelector } from './ModeSelector';
import { AttachMenu } from './AttachMenu';
import { FilePicker } from './FilePicker';
import { PermissionCard } from './PermissionCard';
import { UserInputCard } from './UserInputCard';
import { PolicyInterventionCard } from './PolicyInterventionCard';
import { useT } from '../settings';
import type { MsgKey } from '../i18n';
import {
  applyAtMentionSelection,
  detectAtMentionRange,
  ensureActiveDescendantVisible,
  splitAtToken,
} from '../lib/atMention';
import { toolResultStatus, updateToolProgress, upsertToolPart, type ToolRow, type MsgPart } from '../lib/toolRows';
import { commitTodoCall, todoToolDetail, type TodoTitles } from '../lib/todoToolDetail';
import {
  applyTodoCall,
  isTodoTool,
  projectTodoCalls,
  reduceTodoPanelVisibility,
  type TodoItem,
  type TodoProjectionCall,
} from '../lib/todoState';
import {
  assistantTurnEndFlags,
  isInternalHistoryAssistantMessage,
  isInternalHistoryUserMessage,
  isUserInterruptionMessage,
  shouldShowAssistantTimestamp,
} from '../lib/historyMessages';
import {
  activeTurnSubmissionDisposition,
  chatRecoveryPolicy,
  classifyChatDone,
  createLiveLifecycleState,
  isCurrentChatStream,
  liveDetachDisposition,
  liveSessionSwitchDisposition,
  liveSnapshotQueueDisposition,
  reduceChatRecovery,
  reduceLiveLifecycle,
  resolveUserInputRequest,
  restoreLiveSnapshot,
  syncAttachDisposition,
  type ChatRecoveryEvent,
  type ChatRecoveryState,
} from '../lib/chatTerminal';
import {
  acknowledgeLiveSteers,
  isSteerPending,
  pendingSteersToDraft,
  reconcileSteerReceipt,
  shouldApplySteerProviderFallback,
  type PendingLiveSteer,
} from '../lib/liveSteer';
import { hasCoarsePointer, shouldSendComposerOnEnter } from '../lib/composerKeyboard';
import { completedTurnStats, formatTurnDuration, formatTurnTokens, turnCacheHit } from '../lib/turnStats';
import { disposeNotifications, maybeNotifyTurnFinished } from '../lib/notifications';
import { artifactsByAssistantIndex, type TurnArtifact } from '../lib/turnArtifacts';

interface Message {
  role: 'user' | 'assistant' | 'system';
  parts: MsgPart[];
  images?: ImageData[];
  /** Epoch ms this message was sent/received (PR #562 send-time labels).
   *  Live user messages use submission time and assistant messages receive a
   *  timestamp only at the terminal boundary. Persisted history carries the
   *  corresponding start/completion timestamp from the transcript. Optional so the
   *  type stays backward-compatible with the few code paths that build a
   *  Message literal without a clock (e.g. the queued-placeholder). */
  ts?: number;
  /** Browser-local correlation for an optimistic /live submission. Never persisted. */
  pendingSteerId?: string;
  /** Browser-local: set once any text/reasoning/tool event has touched this
   *  assistant message. Reasoning content is not rendered, so `parts` alone
   *  cannot prove output activity — this flag lets the first reasoning delta
   *  clear the three-dot waiting indicator. Never persisted. */
  outputStarted?: boolean;
}

interface QueuedMessage {
  id: number;
  text: string;
  images?: ImageData[];
  approvalMode?: ApprovalMode;
}

/** Concatenate all text segments (error-detection, skill-title, etc.). */
function messageText(m: Message): string {
  return m.parts.reduce((acc, p) => (p.kind === 'text' ? acc + p.text : acc), '');
}

/** Stable across tabs because every observer receives the same terminal stats. */
function notificationDedupeKey(
  sessionId: string | undefined,
  stopReason: string | undefined,
  stats: TurnStats | undefined,
): string | undefined {
  if (!sessionId || !stats) return undefined;
  return [
    sessionId,
    stopReason ?? 'finished',
    stats.duration_ms,
    stats.rounds,
    stats.tool_calls,
    stats.prompt_tokens,
    stats.completion_tokens,
  ].join(':');
}

/** Zero-pad a local date/time component to 2 digits. */
const pad2 = (n: number) => (n < 10 ? '0' + n : '' + n);

/** Format a message timestamp as a stable, always-visible local wall clock. */
function formatMsgTime(ts: number | undefined): string {
  // P3 修复: 用 ts == null 而非 !ts,避免把 ts=0 (epoch 1970) 误判为无效。
  // 实际消息时间戳不会是 0,但严格区分 "缺失" 与 "值为 0" 更正确。
  if (ts == null || !Number.isFinite(ts)) return '';
  const d = new Date(ts);
  if (isNaN(d.getTime())) return '';
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
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
  /** App-owned `/chat` approval projected into this component's composer seat. */
  pendingPermission?: PermissionRequestEvent | null;
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
function formatToolDetail(name: string, argsJson: string, todoTitles: TodoTitles = new Map()): string {
  const todoDetail = todoToolDetail(name, argsJson, todoTitles);
  if (todoDetail !== null) return todoDetail;
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

export function Chat({ sessionId, onSessionId, cwd, onPermission, pendingPermission, onPermissionResolved, activeSession, restoring, onLiveTurnDone, onOptimisticSession, onOpenCwd, onCwdChanged, onLanding, skillInsert, onSessionRenamed }: ChatProps) {
  const t = useT();
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  // 组合期标记：仅用于 handleKeyDown 的守卫（菜单导航 / Enter 发送不得窃取
  // IME 候选窗的按键）。输入值本身始终受控同步，见 handleInput。
  const composingRef = useRef(false);
  const [busy, setBusyState] = useState(false);
  // Mirror of `busy` in a ref so pushCommandNotice can read it synchronously
  // without a stale closure (refs always reflect the latest render value).
  const busyRef = useRef(false);
  busyRef.current = busy;
  function setBusy(next: boolean) {
    // SSE callbacks and composer events can occur before Preact commits the
    // next render. Keep the ref authoritative synchronously so an active
    // `/chat` turn cannot be mistaken for an idle direct-send window.
    busyRef.current = next;
    setBusyState(next);
  }
  // 回合开始时刻（本 tab 主动 submit 时记录；live 恢复/重放不得重置），
  // 用于 min_duration 过滤与通知正文时长；终态事件计算后即清空。
  const turnStartedAtRef = useRef<number | null>(null);
  // 只有当前 tab 实际提交过回合，或从 /live 观察到 running=true，终态才有
  // 通知资格。初始/重连时重放的 idle state 不能伪装成一次新完成事件。
  const notificationTurnActiveRef = useRef(false);
  function finishTurnNotification(info: {
    stopReason?: string;
    sessionId?: string;
    message?: string;
    durationMs?: number;
    dedupeKey?: string;
  }) {
    if (!notificationTurnActiveRef.current) return;
    const durationMs = info.durationMs ?? (
      turnStartedAtRef.current !== null ? Date.now() - turnStartedAtRef.current : undefined
    );
    notificationTurnActiveRef.current = false;
    turnStartedAtRef.current = null;
    maybeNotifyTurnFinished({ ...info, durationMs });
  }
  const [chatRecovery, setChatRecovery] = useState<ChatRecoveryState>('ready');
  const chatRecoveryRef = useRef<ChatRecoveryState>('ready');
  chatRecoveryRef.current = chatRecovery;
  const recoveryPolicy = chatRecoveryPolicy(chatRecovery);

  function transitionChatRecovery(event: ChatRecoveryEvent): ChatRecoveryState {
    const next = reduceChatRecovery(chatRecoveryRef.current, event);
    chatRecoveryRef.current = next;
    setChatRecovery(next);
    return next;
  }
  // True for the entire duration of a /compact postCommand await so sendMessage
  // can refuse to fire while the session .json is being rewritten on disk.
  const compactingRef = useRef(false);
  // AI 执行中输入的消息排队于此，待当前回合 done 后依次自动发送（对齐 VSCode 插件）。
  const [queued, setQueuedState] = useState<QueuedMessage[]>([]);
  const queuedRef = useRef(queued);
  queuedRef.current = queued;
  function setQueued(
    update: QueuedMessage[] | ((current: QueuedMessage[]) => QueuedMessage[]),
  ) {
    // SSE callbacks can run before Preact commits the next render. Keep the ref
    // authoritative synchronously so a reconnect snapshot cannot miss a just-
    // queued message and accidentally drain it under an unknown terminal.
    const next = typeof update === 'function' ? update(queuedRef.current) : update;
    queuedRef.current = next;
    setQueuedState(next);
  }
  const queueIdRef = useRef(0);
  // Inputs accepted by the live runtime as in-turn steers but not yet folded at
  // a kernel round boundary. Unlike `queued`, these already belong to the active
  // turn and must be recovered if that turn is cancelled before acknowledgement.
  const [pendingSteers, setPendingSteersState] = useState<PendingLiveSteer[]>([]);
  const pendingSteersRef = useRef(pendingSteers);
  pendingSteersRef.current = pendingSteers;
  function setPendingSteers(
    update: PendingLiveSteer[] | ((current: PendingLiveSteer[]) => PendingLiveSteer[]),
  ) {
    const next = typeof update === 'function' ? update(pendingSteersRef.current) : update;
    pendingSteersRef.current = next;
    setPendingSteersState(next);
  }
  function restorePendingSteers(includeSubmitting = true) {
    const pending = pendingSteersRef.current.filter(
      (item) => includeSubmitting || item.confirmed,
    );
    if (pending.length === 0) return;
    const draft = pendingSteersToDraft(pending);
    const pendingIds = new Set(pending.map((item) => item.id));
    setMessages((current) => current.filter((message) => !(
      message.pendingSteerId !== undefined && pendingIds.has(message.pendingSteerId)
    )));
    setInput((current) => [draft.text, current].filter(Boolean).join('\n'));
    setPendingImages((current) => [...draft.images, ...current]);
    setPendingSteers((current) => current.filter((item) => !pendingIds.has(item.id)));
    pushCommandNotice(t('chat.steerRecovered'));
  }
  const [tokens, setTokens] = useState<TokenUsage | null>(null);
  const [turnStats, setTurnStats] = useState<TurnStats | null>(null);
  const [historyHint, setHistoryHint] = useState<string | null>(null);
  // Auxiliary persistence failures belong to application chrome, not the
  // assistant transcript. Replacing this value also deduplicates repeated
  // failures for the same session/path.
  const [persistenceWarning, setPersistenceWarning] = useState<string | null>(null);
  // 正在拉取某会话历史：用于抑制落地页，避免切到「有内容的会话」时先闪一下落地页。
  const [loading, setLoading] = useState(false);
  const [provider, setProvider] = useState<string | null>(null);
  const [reasoningEffort, setReasoningEffort] = useState<{
    provider: string;
    effort: string | null;
  } | null>(null);
  const providerRef = useRef<string | null>(null);
  const providerPinnedRef = useRef(false);
  const followDefaultProvider = useCallback((name: string) => {
    if (!providerPinnedRef.current) {
      providerRef.current = name;
      setProvider(name);
    }
  }, []);
  // 审批模式（build / accept_edits / bypass / plan）。进程级 runtime 状态，
  // 由 /live snapshot + 'mode' 事件同步，切换调 postLiveMode（下一轮生效）。
  // confirmedMode 是 daemon 已确认值。
  const [modeState, setModeState] = useState(() => initModeState('build' as ApprovalMode));
  const [showFilePicker, setShowFilePicker] = useState(false);
  const [pendingImages, setPendingImages] = useState<ImageData[]>([]);
  const [attachError, setAttachError] = useState<string | null>(null);
  const [slashSkills, setSlashSkills] = useState<SkillInfo[] | null>(null);
  const [slashLoading, setSlashLoading] = useState(false);
  const [slashOpen, setSlashOpen] = useState(false);
  const [slashQuery, setSlashQuery] = useState('');
  const [slashIndex, setSlashIndex] = useState(0);
  // The /skills command opens a pure skills browser (no commands). Set while that
  // browser is showing; cleared as soon as the user types (normal mixed filtering).
  const [slashSkillsOnly, setSlashSkillsOnly] = useState(false);
  const [atOpen, setAtOpen] = useState(false);
  const [atQuery, setAtQuery] = useState('');
  const [atIndex, setAtIndex] = useState(0);
  const [atItems, setAtItems] = useState<{ name: string; is_dir: boolean }[]>([]);
  const [atLoading, setAtLoading] = useState(false);
  const [sync, setSync] = useState<boolean>(() => {
    try { return new URLSearchParams(location.search).get('sync') === '1'; } catch { return false; }
  });
  const syncRef = useRef(sync);
  syncRef.current = sync;
  // Pending live-session permission request (shown as PermissionCard, calls /live/permission).
  // Kept separate from the non-sync `onPermission` prop so the /chat path is untouched.
  const [livePending, setLivePending] = useState<{ tool_name: string; reason: string; call_id: string; arguments: string } | null>(null);
  // Pending structured input from either transport. The event's optional session_id
  // selects `/chat/user-input`; live requests answer the bound `/live` runtime.
  const [userInputReq, setUserInputReq] = useState<RoutedUserInputRequest | null>(null);
  const [policyIntervention, setPolicyIntervention] = useState<PolicyInterventionEvent | null>(null);
  const [todoItems, setTodoItems] = useState<TodoItem[]>([]);
  const [todoPreview, setTodoPreview] = useState<TodoItem[] | null>(null);
  const [todoPanelVisible, setTodoPanelVisibleState] = useState(false);
  const todoPanelVisibleRef = useRef(false);
  const todoVisibilityCacheRef = useRef<Map<string, boolean>>(new Map());
  const todoItemsRef = useRef<TodoItem[]>([]);
  const todoCacheRef = useRef<Map<string, TodoItem[]>>(new Map());
  const todoBatchRef = useRef<{
    base: TodoItem[];
    calls: TodoProjectionCall[];
    visibleBase: boolean;
  }>({
    base: [],
    calls: [],
    visibleBase: false,
  });
  const abortRef = useRef<AbortController | null>(null);
  const requestIdRef = useRef<string | null>(null);
  const liveAbortRef = useRef<AbortController | null>(null);
  const liveLifecycleRef = useRef(createLiveLifecycleState());
  // Wall-clock of the last byte received on the /live stream (any event OR the
  // 15s keepalive ping). A watchdog reconnects when this goes stale, catching
  // silently-dead half-open connections after long idle.
  const lastLiveActivityRef = useRef<number>(Date.now());
  // Pending reconnect backoff timer, so teardown can cancel it.
  const reconnectTimerRef = useRef<number | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  // Scroll container + "am I at the bottom?" tracking. Auto-follow during streaming
  // ONLY while the user is at the bottom; scrolling up releases the follow so history
  // stays put. A ref (not state) avoids re-rendering on every scroll event.
  const scrollRef = useRef<HTMLDivElement>(null);
  const atBottomRef = useRef(true);
  const [showJumpBtn, setShowJumpBtn] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const [coarsePointer, setCoarsePointer] = useState(hasCoarsePointer);
  const slashRef = useRef<HTMLDivElement>(null);
  const atRef = useRef<HTMLDivElement>(null);
  // 当前 Chat 正在显示的会话 id。用于区分「外部切换会话(需重置+加载历史)」
  // 与「本次新建会话首条消息完成后自己拿到的 id(不应重置)」。
  const activeIdRef = useRef<string | null>(null);
  // Distinguish async work from A -> B -> A session switches. Comparing only
  // the session id would let the first A's late active-check/cancel result
  // mutate the replacement A view.
  const sessionGenerationRef = useRef(0);
  const renderedSessionIdRef = useRef(sessionId);
  if (renderedSessionIdRef.current !== sessionId) {
    // Advance during render, before a passive effect can abort the old reader.
    // An accepted first-turn `done` pre-binds activeIdRef to its new id, so that
    // normal null -> canonical-id prop update does not invalidate its own tail.
    if (activeIdRef.current !== sessionId) sessionGenerationRef.current += 1;
    renderedSessionIdRef.current = sessionId;
  }
  // 已为哪个 sessionId 触发过历史加载（或它是本 Chat 自建的会话）。用于避免
  // project_hash 迟到（刷新后由 App 异步回填）导致的重复加载 / 覆盖当前对话。
  const loadedForRef = useRef<string | null>(null);
  // Cache of session messages, used to preserve in-progress streaming turns when switching sessions.
  const messageCacheRef = useRef<Map<string, Message[]>>(new Map());
  // Mirror of messages state to read the latest value without dependency tracking.
  const messagesRef = useRef<Message[]>([]);
  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);
  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return;
    const media = window.matchMedia('(pointer: coarse)');
    const update = () => setCoarsePointer(media.matches);
    update();
    media.addEventListener?.('change', update);
    return () => media.removeEventListener?.('change', update);
  }, []);
  // 实时（/live）总线对应的会话 id（来自 snapshot）。用于门控实时事件：仅当用户当前
  // 查看的就是这个实时会话时才把输出渲染进画布——否则用户从侧栏打开了别的历史会话，
  // 实时输出会串进错误页面、且刷新即消失（刷新会按真实会话重载）。
  const liveSessionIdRef = useRef<string | null>(null);
  // 是否已为「当前会话」上报过乐观侧栏条目。每次切换/新建会话时复位，
  // 避免同一会话第二条消息（尤其 sync 路径本地不落消息）重复上报、改写标题。
  const optimisticFiredRef = useRef(false);
  // Sync path: text of a message THIS tab just optimistically appended (so the
  // view leaves the "new conversation" landing page instantly instead of waiting
  // for the server `user` echo, which only arrives after VL preprocessing). The
  // matching self-echo is deduped in the `user` case so we don't render it twice.
  // Turns are serialized (hub `turn_active`), so the next `user` echo after a send
  // is this tab's own; a peer's message (different text, or after the ref clears)
  // still appends normally.
  const pendingSelfEchoRef = useRef<Array<{ id: string; text: string }>>([]);
  // Artifact (code block) streaming: the daemon's ArtifactDetector strips fenced
  // code blocks from TextDelta and emits them as artifact_start / content / end.
  // These refs accumulate the language tag and code body for each artifact so
  // artifact_end can reconstruct the original markdown code block.
  const artifactLangRef = useRef('');
  const artifactBufRef = useRef('');
  const todoTitlesRef = useRef<TodoTitles>(new Map());
  const pendingTodoCallsRef = useRef<Map<string, { name: string; args: string }>>(new Map());

  function setTodoPanelVisible(visible: boolean, persist = true) {
    todoPanelVisibleRef.current = visible;
    setTodoPanelVisibleState(visible);
    const activeId = activeIdRef.current;
    if (persist && activeId) todoVisibilityCacheRef.current.set(activeId, visible);
  }

  function replaceTodoItems(items: readonly TodoItem[], persist = true) {
    const next = items.map((item) => ({ ...item }));
    todoItemsRef.current = next;
    setTodoItems(next);
    const activeId = activeIdRef.current;
    if (persist && activeId) todoCacheRef.current.set(activeId, next);
    todoTitlesRef.current = new Map(next.map((item, index) => [index + 1, item.content]));
  }

  function resetTodoBatch(clearCommitted: boolean, persistClear = true) {
    todoBatchRef.current = {
      base: [],
      calls: [],
      visibleBase: clearCommitted ? false : todoPanelVisibleRef.current,
    };
    pendingTodoCallsRef.current.clear();
    setTodoPreview(null);
    if (clearCommitted) {
      replaceTodoItems([], persistClear);
      setTodoPanelVisible(false, persistClear);
    }
  }

  function stageTodoCall(id: string, name: string, args: string) {
    if (!isTodoTool(name)) return;
    const batch = todoBatchRef.current;
    if (batch.calls.length === 0) {
      batch.base = todoItemsRef.current.map((item) => ({ ...item }));
      batch.visibleBase = todoPanelVisibleRef.current;
    }
    if (!batch.calls.some((call) => call.id === id)) {
      batch.calls.push({ id, name, args });
    }
    const projected = projectTodoCalls(batch.base, batch.calls);
    setTodoPanelVisible(projected.hasApplicable ? true : batch.visibleBase);
    setTodoPreview(projected.hasUnresolved ? projected.preview : null);
  }

  function settleTodoCall(id: string, success: boolean) {
    const batch = todoBatchRef.current;
    const call = batch.calls.find((candidate) => candidate.id === id);
    if (!call) return;
    call.success = success;
    const projected = projectTodoCalls(batch.base, batch.calls);
    setTodoPanelVisible(projected.hasApplicable ? true : batch.visibleBase);
    replaceTodoItems(projected.committed);
    if (projected.hasUnresolved) {
      setTodoPreview(projected.preview);
    } else {
      todoBatchRef.current = {
        base: projected.committed,
        calls: [],
        visibleBase: todoPanelVisibleRef.current,
      };
      setTodoPreview(null);
    }
  }

  function finishTodoTurn(cancelled: boolean) {
    if (cancelled) {
      resetTodoBatch(true);
      return;
    }
    const batch = todoBatchRef.current;
    if (batch.calls.length > 0) {
      replaceTodoItems(projectTodoCalls(batch.base, batch.calls).committed);
    }
    resetTodoBatch(false);
  }

  function restoreTodoTitles(messages: Message[]) {
    todoTitlesRef.current.clear();
    resetTodoBatch(false);
    let restoredItems: TodoItem[] = [];
    for (const message of messages) {
      for (const part of message.parts) {
        if (part.kind === 'tool' && part.tool.status === 'done') {
          commitTodoCall(part.tool.name, part.tool.args, todoTitlesRef.current);
          restoredItems = applyTodoCall(restoredItems, part.tool.name, part.tool.args);
        }
      }
    }
    replaceTodoItems(restoredItems);
    let restoredVisible = false;
    let visibilityItems: TodoItem[] = [];
    for (const message of messages) {
      if (message.role === 'user') {
        restoredVisible = reduceTodoPanelVisibility(restoredVisible, { type: 'user_input' });
      }
      for (const part of message.parts) {
        if (part.kind === 'tool' && part.tool.status === 'done' && isTodoTool(part.tool.name)) {
          const projected = projectTodoCalls(visibilityItems, [{
            id: part.tool.id,
            name: part.tool.name,
            args: part.tool.args,
            success: true,
          }]);
          visibilityItems = projected.committed;
          if (projected.hasApplicable) restoredVisible = true;
        }
      }
    }
    setTodoPanelVisible(restoredVisible);
  }

  // 切换/恢复会话时重置画布并加载历史。依赖 project_hash：刷新后 sessionId 先于
  // 元数据就绪，此时只显示提示；待 App 从会话列表回填 project_hash，本 effect 因
  // 依赖变化重跑，再真正拉取历史。
  useEffect(() => {
    // 会话 id 变化（外部切换 / 新建按钮）才重置画布。本 Chat 自建会话首条消息完成后
    // sessionId 变成自己的 id（activeIdRef 已同步），不重置，以免清空刚看到的对话。
    if (sessionId !== activeIdRef.current) {
      const switchGeneration = sessionGenerationRef.current;
      const prevId = activeIdRef.current;
      const liveSwitch = liveSessionSwitchDisposition(
        liveLifecycleRef.current.running || busyRef.current,
      );
      if (syncRef.current && !liveSwitch.allowed) {
        if (prevId) onSessionId(prevId);
        pushCommandNotice(t('cmd.session.busy'));
        return;
      }
      // A correlated steer still belongs to the session being left. Recover it
      // while that session is authoritative and reject this switch once; moving
      // the draft after `activeIdRef` changes would leak it into another chat.
      if (pendingSteersRef.current.length > 0) {
        const pendingOwnerId = prevId ?? liveSessionIdRef.current;
        if (pendingOwnerId) {
          restorePendingSteers();
          onSessionId(pendingOwnerId);
          return;
        }
      }
      const detachedRequestId = requestIdRef.current;
      const detachedController = abortRef.current;
      if (prevId && messagesRef.current.length > 0) {
        messageCacheRef.current.set(prevId, messagesRef.current);
      }

      activeIdRef.current = sessionId;
      todoTitlesRef.current.clear();
      resetTodoBatch(true, false);
      providerPinnedRef.current = false;
      loadedForRef.current = null;
      optimisticFiredRef.current = false;
      pendingSelfEchoRef.current = [];
      // An existing session stays send-locked until `/chat/active` proves it
      // has no detached operation. Its canonical id is already a safe stop
      // alias, including while the discovery request is pending or unavailable.
      requestIdRef.current = sessionId;
      abortRef.current = null;
      transitionChatRecovery({ type: 'session_switch', hasSession: sessionId !== null });
      if (!syncRef.current && detachedRequestId) {
        const cancellation = detachedController
          ? cancelDetachedChat(detachedRequestId, detachedController)
          : stopChat(detachedRequestId);
        void cancellation.catch((error) => {
          // A -> B -> A can reuse an id. Generation, not id equality, keeps a
          // late cancellation failure out of the replacement view.
          if (sessionGenerationRef.current === switchGeneration) {
            pushCommandNotice(t('chat.cancelFailed', { error: String(error) }));
          }
        });
      } else if (!syncRef.current) {
        detachedController?.abort();
      }
      liveLifecycleRef.current = createLiveLifecycleState();
      notificationTurnActiveRef.current = false;
      turnStartedAtRef.current = null;
      disposeNotifications();
      setBusy(false);
      setQueued([]);
      setLivePending(null);
      setUserInputReq(null);
      setPolicyIntervention(null);
      onPermissionResolved?.(null);

      const cached = sessionId ? messageCacheRef.current.get(sessionId) : undefined;
      if (cached && cached.length > 0) {
        const cachedTodos = sessionId ? todoCacheRef.current.get(sessionId) : undefined;
        const cachedTodoVisibility = sessionId
          ? todoVisibilityCacheRef.current.get(sessionId)
          : undefined;
        restoreTodoTitles(cached);
        if (cachedTodos) replaceTodoItems(cachedTodos);
        if (cachedTodoVisibility !== undefined) setTodoPanelVisible(cachedTodoVisibility);
        setMessages(cached);
      } else {
        setMessages([]);
      }

      // bot review P2: 切换会话时重置搜索状态,避免残留关键词过滤新会话、matchIdx 超界致计数错乱。
      setSearch('');
      setMatchIdx(0);
      setTokens(null);
      setTurnStats(null);
      setHistoryHint(null);
      setPersistenceWarning(null);
      // 切到一个有 id 的会话 → 进入「加载中」，先抑制落地页（避免闪屏）；
      // 无 id（新建）则不加载、直接落地。
      setLoading(sessionId != null && !cached);
      // sync 模式：本端在侧栏切到另一个（已存在）会话时，通知后端广播会话切换，
      // 使同进程 sync 模式的 TUI 跟随加载该会话历史。
      // 不会回环：远端 session_switched 事件的 handler 会先把 activeIdRef 设为该 id，
      // 故由广播回流引起的 sessionId 变化进不来这个分支（条件已不成立），不会再次广播。
      if (sync && sessionId) {
        postLiveSwitchSession(sessionId)
          .then((result) => {
            if (result.ok || sessionGenerationRef.current !== switchGeneration) return;
            if (result.activeTurn && prevId) {
              onSessionId(prevId);
              pushCommandNotice(t('cmd.session.busy'));
              return;
            }
            throw new Error(result.error ?? 'live runtime rejected the session switch');
          })
          .catch((error) => {
            if (sessionGenerationRef.current !== switchGeneration) return;
            stopLiveStream();
            setSync(false);
            setBusy(false);
            setQueued([]);
            setHistoryHint(t('sync.switchFailed', { error: String(error) }));
          });
      }
    }

    if (!sessionId) {
      requestIdRef.current = null;
      return;
    }
    // 已为该会话加载过历史（或它是本 Chat 自建会话）→ 不重复加载、不覆盖。
    if (loadedForRef.current === sessionId) return;

    const projectHash = activeSession?.project_hash;
    if (!projectHash || (activeSession && activeSession.id !== sessionId)) {
      // 还没拿到对齐的 project_hash / activeSession：先给「继续会话」提示，等其到位再由本 effect 重跑加载。
      setHistoryHint(t('chat.continueSession', { id: sessionId.slice(0, 8) }));
      setLoading(false);
      return;
    }

    // 标记已为该会话发起加载，避免并发/重复。
    loadedForRef.current = sessionId;
    const cached = messageCacheRef.current.get(sessionId);
    if (!cached) {
      setLoading(true);
    }
    const loadId = sessionId;
    const loadGeneration = sessionGenerationRef.current;
    Promise.allSettled([getSession(projectHash, loadId), getActiveChatSessions()])
      .then(([sessionResult, activeResult]) => {
        // Generation also covers A -> B -> A; id equality alone is insufficient.
        if (
          activeIdRef.current !== loadId ||
          sessionGenerationRef.current !== loadGeneration
        ) return;

        let nextHint: string | null = null;
        if (sessionResult.status === 'fulfilled' && sessionResult.value && Array.isArray(sessionResult.value.messages)) {
          const currentCached = messageCacheRef.current.get(loadId);
          const cachedTodos = todoCacheRef.current.get(loadId);
          const cachedTodoVisibility = todoVisibilityCacheRef.current.get(loadId);
          // Convert loaded messages to display format (reuses sessionMessagesToDisplay).
          const loaded = sessionMessagesToDisplay(sessionResult.value.messages);

          if (currentCached && currentCached.length > 0) {
            // If the loaded messages from the backend are shorter than the cached messages,
            // it means the backend task is still running or hasn't saved yet.
            // Keep the cached messages and do not overwrite them with stale backend history.
            if (loaded.length >= currentCached.length) {
              setMessages(loaded);
              messageCacheRef.current.delete(loadId);
            } else {
              // `sessionMessagesToDisplay` also projects Todo state. If the
              // persisted transcript loses to our longer local cache, restore
              // the matching local Todo projection as well; otherwise the
              // canvas and task card would represent different revisions.
              restoreTodoTitles(currentCached);
              if (cachedTodos) replaceTodoItems(cachedTodos);
              if (cachedTodoVisibility !== undefined) {
                setTodoPanelVisible(cachedTodoVisibility);
              }
            }
          } else if (loaded.length > 0) {
            // A newly loaded session starts at the bottom regardless of prior scroll state.
            atBottomRef.current = true;
            setMessages(loaded);
          }
        } else {
          loadedForRef.current = null;
          nextHint = t('chat.continueSession', { id: loadId.slice(0, 8) });
        }

        if (activeResult.status === 'fulfilled') {
          const active = activeResult.value.includes(loadId);
          transitionChatRecovery({ type: 'active_check_succeeded', active });
          if (active) {
            requestIdRef.current = loadId;
            if (!syncRef.current) setBusy(false);
            setQueued([]);
            nextHint = t('chat.detachedActive');
            pushCommandNotice(t('chat.detachedActive'));
          } else if (requestIdRef.current === loadId) {
            requestIdRef.current = null;
          }
        } else {
          // `/chat` has no stream reattach endpoint. The registry is authoritative
          // when available; if discovery fails, keep the canonical stop alias and
          // fail closed instead of allowing a possibly concurrent send.
          requestIdRef.current = loadId;
          transitionChatRecovery({ type: 'active_check_failed' });
          if (!syncRef.current) setBusy(false);
          setQueued([]);
          nextHint = t('chat.activeCheckFailed', { error: String(activeResult.reason) });
          pushCommandNotice(nextHint);
          // Allow a later metadata/session refresh to retry discovery.
          loadedForRef.current = null;
        }

        setHistoryHint(nextHint);
        setLoading(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, activeSession?.project_hash]);

  // How close to the bottom (px) still counts as "at the bottom" — a small slack so
  // sub-pixel rounding / layout jitter during streaming doesn't wrongly release follow.
  const BOTTOM_SLACK = 80;
  const recomputeAtBottom = () => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_SLACK;
    atBottomRef.current = atBottom;
    setShowJumpBtn((v) => (v === !atBottom ? v : !atBottom));
  };
  const scrollToBottom = (behavior: ScrollBehavior = 'auto') => {
    atBottomRef.current = true;
    setShowJumpBtn(false);
    // Completion stats belong to the previous turn. Clear them as soon as a
    // new message is actually delivered so stale data is never shown while
    // the next turn is running or after an early transport failure.
    setTurnStats(null);
    bottomRef.current?.scrollIntoView({ behavior });
  };

  // Follow streaming output ONLY while the user is at the bottom. Scrolling up
  // releases the follow (recomputeAtBottom on the container's scroll event sets
  // atBottomRef=false), so history stays put mid-stream. Instant behavior so the
  // programmatic scroll lands immediately and the next scroll event reads "at bottom".
  useEffect(() => {
    if (atBottomRef.current) bottomRef.current?.scrollIntoView({ behavior: 'auto' });
  }, [messages, tokens, queued.length]);

  // Abort the live (/live) stream + cancel any pending reconnect timer if the
  // component unmounts while sync is on.
  useEffect(() => () => {
    liveAbortRef.current?.abort();
    if (reconnectTimerRef.current !== null) clearTimeout(reconnectTimerRef.current);
  }, []);

  useEffect(() => {
    let cancelled = false;
    getApprovalMode()
      .then((current) => {
        if (!cancelled) setModeState(initModeState(current));
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, []);

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
    if (reconnectTimerRef.current !== null) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    const controller = new AbortController();
    liveAbortRef.current = controller;
    lastLiveActivityRef.current = Date.now();

    // /live can silently die after a long idle (proxy / OS TCP timeout), and
    // the daemon stays alive — so a prompt sent afterwards reaches the daemon
    // (the synced TUI shows it) but its `user` echo never arrives here and the
    // message is missing in the webui. On stream death we RECONNECT instead of
    // dropping sync: the fresh snapshot re-renders the whole conversation,
    // recovering anything sent while the stream was down, and resumes live
    // events. Backoff caps the retry rate; after a run of failures (daemon
    // genuinely gone) we fall back to non-sync so the user isn't stuck.
    let attempt = 0;
    const scheduleReconnect = (startedAt: number) => {
      // Superseded by a newer stream (session switch / manual toggle / watchdog)
      // — that one owns reconnection now.
      if (controller.signal.aborted) return;
      // A connection that stayed up a while was healthy → reset the backoff so a
      // later idle-death starts fresh. A connect that dies immediately keeps
      // escalating, so a broken/flapping daemon backs off and eventually falls
      // to non-sync instead of hammering ~1s reconnects (each re-snapshots the
      // whole conversation). NB: do NOT reset on every byte — the snapshot's
      // first byte would pin backoff at 1s forever.
      if (Date.now() - startedAt >= 30000) attempt = 0;
      attempt += 1;
      if (attempt > 6) {
        stopLiveStream();
        liveLifecycleRef.current = createLiveLifecycleState();
        restorePendingSteers();
        setSync(false);
        setBusy(false);
        setQueued([]);
        setLivePending(null);
        setUserInputReq(null);
        setHistoryHint(t('sync.reconnectFailed'));
        pushNoticeToLastAssistant(t('sync.reconnectFailed'));
        return;
      }
      const delay = Math.min(1000 * 2 ** (attempt - 1), 15000);
      reconnectTimerRef.current = window.setTimeout(() => {
        reconnectTimerRef.current = null;
        if (!controller.signal.aborted) run();
      }, delay);
    };
    const run = () => {
      const startedAt = Date.now();
      streamLive(
        (event) => {
          // Abort alone does not retract a callback already queued by the old
          // reader. Controller identity is the live-stream generation fence.
          if (liveAbortRef.current === controller && !controller.signal.aborted) {
            onLiveEvent(event);
          }
        },
        controller.signal,
        activeIdRef.current,
        () => {
          // Heartbeat for the staleness watchdog (any byte incl. keepalive ping).
          lastLiveActivityRef.current = Date.now();
        },
      )
        .then(() => scheduleReconnect(startedAt)) // clean server close → reconnect
        .catch(() => scheduleReconnect(startedAt)); // error → reconnect
    };
    run();
  }

  function stopLiveStream() {
    liveAbortRef.current?.abort();
    liveAbortRef.current = null;
    if (reconnectTimerRef.current !== null) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
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

  // 实时流保活看门狗：daemon 每 15s 发一次 keepalive ping，健康连接至少每
  // 15s 有字节。若 45s（约 3 个 ping）无任何字节，说明连接已「静默半开」
  // （长时间空闲后被代理/OS 掐断却没有 FIN，reader.read() 会一直挂着，既不
  // 报错也收不到消息 —— 正是「隔很久后发消息 webui 不显示、TUI 却有」的成因）。
  // 此时主动重连：abort 会解开挂起的 read，新连接的 snapshot 重绘整段对话，
  // 找回期间漏收的消息并恢复实时事件。
  useEffect(() => {
    if (!sync) return;
    const id = setInterval(() => {
      if (Date.now() - lastLiveActivityRef.current > 45000) {
        // startLiveStream 会先 abort 旧流再重连（内部已处理重入）。
        startLiveStream();
      }
    }, 15000);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sync]);

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
    const todoTitles: TodoTitles = new Map();
    const pendingTodoCalls = new Map<string, { name: string; args: string }>();
    const orderedTodoCalls: TodoProjectionCall[] = [];
    const todoCallEpochs = new Map<string, number>();
    let inputEpoch = 0;
    for (const msg of msgs) {
      if (msg.role === 'user') {
        if (isUserInterruptionMessage(msg)) {
          orderedTodoCalls.length = 0;
          pendingTodoCalls.clear();
          todoCallEpochs.clear();
          inputEpoch += 1;
          continue;
        }
        if (isInternalHistoryUserMessage(msg.content ?? '', msg.synthetic)) continue;
        inputEpoch += 1;
        loaded.push({
          role: 'user',
          parts: [{ kind: 'text', text: stripVisionAnnotation(msg.content ?? '') }],
          images: msg.images && msg.images.length ? msg.images : undefined,
          ts: msg.created_at,
        });
      } else if (msg.role === 'assistant') {
        if (isInternalHistoryAssistantMessage(msg)) continue;
        // Text comes first (the LLM speaks, then calls tools), so the part
        // order for a persisted round is [text, tool, tool, …].
        const parts: MsgPart[] = [];
        if (msg.content) parts.push({ kind: 'text', text: msg.content });
        for (const tc of msg.tool_calls ?? []) {
          const args = tc.arguments || tc.display || '';
          parts.push({
            kind: 'tool',
            tool: {
              id: tc.id,
              name: tc.name,
              args,
              detail: formatToolDetail(tc.name, args, todoTitles),
              status: 'done',
            },
          });
          pendingTodoCalls.set(tc.id, { name: tc.name, args });
          if (isTodoTool(tc.name)) {
            orderedTodoCalls.push({ id: tc.id, name: tc.name, args });
            todoCallEpochs.set(tc.id, inputEpoch);
          }
        }
        loaded.push({ role: 'assistant', parts, ts: msg.created_at });
      } else if (msg.role === 'tool' && msg.tool_result) {
        const result = msg.tool_result;
        outer: for (let i = loaded.length - 1; i >= 0; i--) {
          const m = loaded[i];
          if (m.role !== 'assistant') continue;
          for (const p of m.parts) {
            if (p.kind === 'tool' && p.tool.id === result.call_id) {
              p.tool.output = result.summary;
              p.tool.status = toolResultStatus(result.success, result.summary);
              const pending = pendingTodoCalls.get(result.call_id);
              if (pending && isTodoTool(pending.name)) {
                const call = orderedTodoCalls.find((candidate) => candidate.id === result.call_id);
                if (call) {
                  call.success = result.success;
                }
                const projected = projectTodoCalls([], orderedTodoCalls);
                todoTitles.clear();
                projected.committed.forEach((item, index) => {
                  todoTitles.set(index + 1, item.content);
                });
              }
              pendingTodoCalls.delete(result.call_id);
              break outer;
            }
          }
        }
      }
      // system messages: skip
    }
    const restoredProjection = projectTodoCalls([], orderedTodoCalls);
    const restoredTodoItems = restoredProjection.committed;
    const priorTodoCalls = orderedTodoCalls.filter(
      (call) => (todoCallEpochs.get(call.id) ?? 0) < inputEpoch,
    );
    const currentTodoCalls = orderedTodoCalls.filter(
      (call) => todoCallEpochs.get(call.id) === inputEpoch,
    );
    const priorTodoItems = projectTodoCalls([], priorTodoCalls).committed;
    const restoredTodoVisible = projectTodoCalls(
      priorTodoItems,
      currentTodoCalls,
    ).hasApplicable;
    todoTitlesRef.current = new Map(
      restoredTodoItems.map((item, index) => [index + 1, item.content]),
    );
    pendingTodoCallsRef.current = pendingTodoCalls;
    todoBatchRef.current = restoredProjection.hasUnresolved
      ? { base: [], calls: orderedTodoCalls, visibleBase: false }
      : { base: restoredTodoItems, calls: [], visibleBase: restoredTodoVisible };
    setTodoPreview(restoredProjection.hasUnresolved ? restoredProjection.preview : null);
    replaceTodoItems(restoredTodoItems);
    setTodoPanelVisible(
      restoredTodoVisible
        && (restoredProjection.preview.length > 0 || restoredTodoItems.length > 0),
    );
    return loaded;
  }

  // ── Live SSE adapter: map LiveWireEvent → SSEEvent (for variants that overlap) ──
  function liveToSSE(e: LiveWireEvent): SSEEvent | null {
    switch (e.type) {
      case 'text': return { type: 'text', content: e.content };
      case 'reasoning': return { type: 'reasoning', content: e.content };
      case 'tool_start': return { type: 'tool_start', id: e.id, name: e.name, arguments: e.arguments };
      case 'tool_output': return { type: 'tool_output', chunk: e.chunk };
      case 'tool_progress': return { type: 'tool_progress', id: e.id, progress: e.progress };
      case 'tool_result': return { type: 'tool_result', id: e.id, name: e.name, output: e.output, success: e.success, duration_ms: e.duration_ms };
      case 'tokens': return { type: 'tokens', prompt: e.prompt, completion: e.completion, total: e.total };
      case 'warning': return { type: 'warning', message: e.message };
      case 'persistence_warning': return { type: 'persistence_warning', message: e.message };
      case 'rate_limited': return { type: 'rate_limited', reset_at_display: e.reset_at_display, reset_label: e.reset_label, secs_until_reset: e.secs_until_reset, auto_resuming: e.auto_resuming, server_message: e.server_message ?? null };
      // NOTE: no artifact_* mapping. This is safe today because the /sync live
      // wire forwards raw TextDelta with the ``` fences intact (see to_wire in
      // live_api.rs — it does NOT run text through ArtifactDetector), so the
      // Markdown renderer sees code blocks directly. The /chat path strips them
      // into artifact_* events, which handleEvent reconstructs. If the live path
      // ever adopts the ArtifactDetector, add artifact_start/content/end here or
      // /sync will silently drop fenced code again.
      case 'command_output': return { type: 'command_output', text: e.text };
      default: return null;
    }
  }

  // ── Live event handler ──
  function onLiveEvent(e: LiveWireEvent) {
    // snapshot：确立实时会话 id 并把视图切到它（连上即对齐）。
    if (e.type === 'snapshot') {
      liveSessionIdRef.current = e.session_id || null;
      // Historical timestamps come from the daemon's native transcript projection.
      // Older sessions may not have transcript records; leave those timestamps absent
      // instead of relabelling the whole conversation with the reconnect wall clock.
      const loaded = sessionMessagesToDisplay(e.messages);
      // Live snapshot (session switch / reconnect) → start at the bottom.
      atBottomRef.current = true;
      // A persisted snapshot does not carry live activity. Replay emits the
      // authoritative input/state observations immediately after it; only those
      // may restore `busy`. In particular, a history ending in a user message can
      // be an already-failed turn and must not manufacture an in-flight reply.
      const queueDisposition = liveSnapshotQueueDisposition(
        liveLifecycleRef.current.running || busyRef.current,
        queuedRef.current.length,
      );
      const lifecycle = reduceLiveLifecycle(liveLifecycleRef.current, { type: 'snapshot' });
      liveLifecycleRef.current = lifecycle.state;
      const restored = restoreLiveSnapshot(loaded);
      setMessages(restored.messages.length > 0 ? restored.messages : []);
      setBusy(restored.running);
      if (queueDisposition.discardQueued) {
        setQueued([]);
        pushCommandNotice(t('sync.reconnectTerminalUnknown'));
      }
      setLivePending(null);
      setUserInputReq(null);
      setHistoryHint(null);
      // 连上时回显当前生效的模型，让下拉框与 TUI / 其他端保持一致。
      if (e.provider && !providerPinnedRef.current) {
        providerRef.current = e.provider;
        setProvider(e.provider);
      }
      // 同步当前审批模式，让新 tab 显示正确的模式 pill（含别的 tab 切成的 Auto/Plan）。
      if (e.mode) setModeState(initModeState(e.mode));
      // 把稳定的 session_id 告知 App，接入侧边栏历史 + URL 刷新恢复。
      // 与 /chat 的 'done' 事件同路径：activeIdRef + loadedForRef 标记，
      // 避免 App 回填 project_hash 时触发重复加载覆盖当前画布。
      if (e.session_id) {
        if (activeIdRef.current !== e.session_id) {
          sessionGenerationRef.current += 1;
        }
        activeIdRef.current = e.session_id;
        loadedForRef.current = e.session_id;
        onSessionId(e.session_id);
      }
      return;
    }
    // 模型切换是进程级（全局），与正在查看哪个会话无关。provider 事件是运行时
    // ProviderChanged 的权威回声（本端确认的切换 / 别的 tab / TUI），一律采纳并解除
    // pin——pin 只用于防止「重连快照」回退一次尚未传播的本地切换，不该挡真实的变更事件，
    // 否则本端切过一次模型后就永远不再跟随 TUI 的模型切换（S1）。
    if (e.type === 'provider') {
      providerRef.current = e.provider;
      setProvider(e.provider);
      providerPinnedRef.current = false;
      return;
    }
    if (e.type === 'reasoning_effort') {
      if (providerRef.current === e.provider) {
        setReasoningEffort({ provider: e.provider, effort: e.effort });
      }
      return;
    }
    // 审批模式切换是进程级（另一 tab / 未来 TUI）→ 始终同步 pill。
    if (e.type === 'mode') {
      setModeState(initModeState(e.mode));
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
    // 重启 SSE 连接以取得新会话的 authoritative snapshot 和 replay window。
    if (e.type === 'session_switched') {
      liveSessionIdRef.current = e.session_id;
      // 本端正是发起此次切换的视图（侧栏切到已存在会话→postLiveSwitchSession→广播
      // 回流），或重复广播：此时我们已经在该会话上、历史也正在/已经加载，绝不能再清空
      // 画布，否则刚 getSession 加载好的历史会被自己的广播回流抹掉（且 sessionId 未变，
      // 加载 effect 不会重跑，history 永远回不来）。仅刷新实时流即可。
      const alreadyViewing = activeIdRef.current === e.session_id;
      activeIdRef.current = e.session_id;
      if (!alreadyViewing) {
        sessionGenerationRef.current += 1;
        onSessionId(e.session_id);
        todoTitlesRef.current.clear();
        resetTodoBatch(true, false);
        setMessages([]);
        // bot review P2: 切换会话时重置搜索状态,避免残留关键词过滤新会话、matchIdx 超界致计数错乱。
        setSearch('');
        setMatchIdx(0);
        setSearchOpen(false);
        setTokens(null);
        setTurnStats(null);
        setHistoryHint(null);
        setPersistenceWarning(null);
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
        const wasRunning = liveLifecycleRef.current.running;
        if (!wasRunning) {
          // A newly accepted input starts a new turn before the following
          // `state { running: true }` projection necessarily arrives. Retire
          // the previous completed-turn footer at this authoritative boundary.
          setTurnStats(null);
          setTodoPanelVisible(reduceTodoPanelVisibility(
            todoPanelVisibleRef.current,
            { type: 'user_input' },
          ));
        }
        const lifecycle = reduceLiveLifecycle(liveLifecycleRef.current, {
          type: 'input_accepted',
        });
        liveLifecycleRef.current = lifecycle.state;
        setBusy(lifecycle.state.running);
        // Dedup THIS tab's own echo: we already optimistically appended it on send
        // (leaving the landing page instantly). Consume the marker and skip the
        // re-append; a peer's message still falls through and renders.
        const ownEchoIndex = e.client_input_id
          ? pendingSelfEchoRef.current.findIndex((pending) => pending.id === e.client_input_id)
          : pendingSelfEchoRef.current[0]?.text === e.text ? 0 : -1;
        if (ownEchoIndex >= 0) {
          pendingSelfEchoRef.current.splice(ownEchoIndex, 1);
          break;
        }
        // Append the peer's user message + empty assistant placeholder
        const now = Date.now();
        setMessages((prev) => [
          ...prev,
          { role: 'user', parts: [{ kind: 'text', text: e.text }], images: e.images && e.images.length ? e.images : undefined, ts: now },
          { role: 'assistant', parts: [] },
        ]);
        break;
      }

      case 'state': {
        if (e.running && !notificationTurnActiveRef.current) {
          notificationTurnActiveRef.current = true;
          turnStartedAtRef.current = Date.now();
        }
        const lifecycle = reduceLiveLifecycle(liveLifecycleRef.current, {
          type: 'state',
          running: e.running,
          stopReason: e.stop_reason,
          message: e.message,
        });
        liveLifecycleRef.current = lifecycle.state;
        const terminal = lifecycle.terminal;
        setBusy(lifecycle.state.running);
        if (e.running) setTurnStats(null);
        else if (terminal) setTurnStats(completedTurnStats(e.stats, e.stop_reason));
        if (terminal) {
          stampLastAssistantCompletion();
          finishTodoTurn(e.stop_reason === 'cancelled');
          if (terminal.discardQueued) {
            // Abnormal terminal (cancel / error): the kernel CLEARS its steer
            // buffer (agent.rs Cancel/Shutdown), so a leftover steer is NOT
            // re-run. Recover its text to the composer instead of stranding the
            // optimistic bubble with no re-run to fill it.
            restorePendingSteers(false);
            setQueued([]);
            pushNoticeToLastAssistant(t('chat.incomplete', { msg: terminal.detail }));
          } else {
            // Normal completion: the kernel re-runs any steer that did not fold
            // as the next turn (agent.rs leftover-steer drain), so drop the
            // pending markers and let that authoritative re-run reconcile the
            // optimistic echo — matching the TUI, which never returns the text to
            // the input. Unconfirmed steers keep their marker; their in-flight
            // receipt lands as `release` (terminal now consumed) and drops it.
            setPendingSteers((pending) => pending.filter((item) => !item.confirmed));
          }
          // 回合结束（idle）时不可能再有待批准项：清掉因对端(TUI)批准或回合收尾而
          // 残留的审批卡片，否则 webui 会一直挂着一张「等待批准…」的卡片直到刷新。
          setLivePending(null);
          setUserInputReq(null);
          if (e.stop_reason !== 'policy_denied') setPolicyIntervention(null);
          // 回合结束：计算通知信息并触发浏览器提醒（若守卫通过）。
          finishTurnNotification({
            stopReason: e.stop_reason,
            sessionId: liveSessionIdRef.current ?? undefined,
            message: e.message,
            durationMs: e.stats?.duration_ms,
            dedupeKey: notificationDedupeKey(
              liveSessionIdRef.current ?? undefined,
              e.stop_reason,
              e.stats,
            ),
          });
          // turn 完成后 session 已落盘，通知 App 刷新侧栏列表。
          onLiveTurnDone?.();
        }
        break;
      }
      case 'steered': {
        setPendingSteers((pending) => acknowledgeLiveSteers(
          pending,
          e.inputs,
          e.client_input_ids,
        ));
        break;
      }
      case 'error': {
        const lifecycle = reduceLiveLifecycle(liveLifecycleRef.current, {
          type: 'error',
          message: e.message,
        });
        liveLifecycleRef.current = lifecycle.state;
        pushNoticeToLastAssistant(t('chat.error', { msg: lifecycle.diagnostic ?? e.message }));
        break;
      }
      case 'permission_request': {
        // Mark the tool row as waiting for approval (same as non-sync path)
        updateToolInLastAssistant(e.call_id, { status: 'waiting_approval' });
        // Show the PermissionCard for the live session (calls /live/permission via onDecide)
        setLivePending({ tool_name: e.tool_name, reason: e.reason, call_id: e.call_id, arguments: e.arguments });
        break;
      }
      case 'user_input_request': {
        // Show the UserInputCard for the bound live runtime.
        setUserInputReq({ ...e, response_transport: 'live' });
        break;
      }
      case 'user_input_resolved': {
        setUserInputReq((current) => resolveUserInputRequest(current, e.request_id));
        break;
      }
      case 'policy_intervention': {
        setPolicyIntervention(e);
        break;
      }
      case 'policy_intervention_resolved': {
        setPolicyIntervention((current) => current?.intervention_id === e.intervention_id ? null : current);
        break;
      }
      case 'policy_intervention_cleared': {
        setPolicyIntervention((current) => current?.intervention_id === e.intervention_id ? null : current);
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
        const attach = syncAttachDisposition(
          busyRef.current,
          chatRecoveryRef.current,
        );
        if (!attach.allowed) {
          pushCommandNotice(t('sync.stopBeforeAttach'));
          return prev;
        }
      }
      if (!next) {
        const detach = liveDetachDisposition(
          liveLifecycleRef.current.running || busyRef.current,
        );
        if (!detach.allowed) {
          pushNoticeToLastAssistant(t('sync.stopBeforeDetach'));
          return prev;
        }
      }
      if (next) {
        // 重新开启同步：先让已绑定的 Coding Runtime 恢复当前会话，再读取 live hub 快照。
        const sid = activeIdRef.current;
        if (sid) {
          postLiveSwitchSession(sid)
            .then((result) => {
              if (!result.ok) {
                throw new Error(result.error ?? 'live runtime rejected the session switch');
              }
              startLiveStream();
            })
            .catch((error) => {
              setSync(false);
              setHistoryHint(t('sync.switchFailed', { error: String(error) }));
            });
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

  /** Stamp the assistant when the turn actually finishes. The optimistic
   *  placeholder intentionally has no clock, keeping reply completion distinct
   *  from the user's submission time. */
  function stampLastAssistantCompletion(ts = Date.now()) {
    setMessages((prev) => {
      for (let index = prev.length - 1; index >= 0; index -= 1) {
        if (prev[index].role !== 'assistant') continue;
        const next = prev.slice();
        next[index] = { ...next[index], ts };
        return next;
      }
      return prev;
    });
  }

  // 命令输出以独立 system 消息追加进转录，与 assistant 消息分离。
  // 若流式 assistant 回复正在进行（busy），把 notice 插到它之前，
  // 确保 appendToLastAssistant 始终以真正的 assistant 气泡为最后一条。
  function pushCommandNotice(text: string) {
    setMessages((prev) => {
      const note: Message = { role: 'system', parts: [{ kind: 'notice', text }], ts: Date.now() };
      const last = prev[prev.length - 1];
      // Keep an in-flight streaming assistant reply as the last element so
      // appendToLastAssistant still targets IT, not this notice.
      if (last && last.role === 'assistant' && busyRef.current) {
        return [...prev.slice(0, -1), note, last];
      }
      return [...prev, note];
    });
  }

  const slashCommandMap = useMemo(() => buildCommandMap(FRONTEND_COMMANDS), []);

  // Reload one session's transcript from disk into the view, guarded against a
  // session switch racing the async fetch. Mirrors the session-load effect's activeIdRef guard.
  async function reloadSessionTranscript(id: string) {
    const projectHash = activeSession?.project_hash;
    if (!projectHash) return;
    try {
      const detail = await getSession(projectHash, id);
      if (activeIdRef.current === id) {
        setMessages(sessionMessagesToDisplay(detail.messages));
      }
    } catch { /* refresh failure is non-fatal; the notice still gives feedback */ }
  }

  // ── Shared mode / provider switch helpers ──────────────────────────────────
  // Defined in render scope (not memoized) so they always close over the latest
  // modeState / sync. Both call sites — ModeSelector.onChange / slashHandlers
  // and ModelSelector.onChange / slashHandlers — call these directly.

  /** Initiate a mode switch. onConfirmed is called with the server-confirmed
   *  mode after the backend round-trip completes (used by slash commands to
   *  emit a notice with the ACTUAL confirmed mode, not the requested one). */
  function switchMode(m: ApprovalMode, onConfirmed?: (confirmed: ApprovalMode) => void) {
    if (modeState.pendingMode) return;
    const nextState = beginModeSwitch(modeState, m);
    if (nextState === modeState) return;
    setModeState(nextState);
    void postLiveMode(m)
      .then((confirmed) => {
        setModeState((cur) => completeModeSwitch(cur, confirmed));
        onConfirmed?.(confirmed);
      })
      .catch(() => setModeState((cur) => failModeSwitch(cur)));
  }

  /** Switch the active provider and notify the backend when in sync mode. */
  function switchProvider(name: string) {
    providerPinnedRef.current = true;
    if (!sync) {
      providerRef.current = name;
      setProvider(name);
      return;
    }
    const previous = provider;
    providerRef.current = name;
    setProvider(name);
    void postLiveProvider(name, sessionId).then((res) => {
      if (res.ok) return;
      if (providerRef.current !== name) return;
      // Rejected (e.g. a turn is running): undo the optimistic selection and unpin
      // so the selector resumes following the runtime's authoritative model.
      providerRef.current = previous;
      setProvider(previous);
      providerPinnedRef.current = false;
      pushCommandNotice(res.activeTurn ? t('cmd.model.syncBusy') : (res.error ?? t('cmd.model.syncBusy')));
    }).catch((error) => {
      if (providerRef.current !== name) return;
      providerRef.current = previous;
      setProvider(previous);
      providerPinnedRef.current = false;
      setHistoryHint(t('chat.connError', { msg: String(error) }));
    });
  }

  const slashHandlers: SlashHandlers = useMemo(
    () => ({
      // setMode: 委托给 switchMode；notice 用后端确认的模式（非请求的模式）。
      setMode: (m) => {
        switchMode(m, (confirmed) => pushCommandNotice(t('cmd.mode.done', { mode: confirmed })));
      },
      // openModelPicker: ModelSelector 是自包含组件，无法从外部以编程方式打开。
      openModelPicker: () => { pushCommandNotice(t('cmd.model.openHint')); },
      // setProvider: 委托给 switchProvider（实时同步 + 本地状态）。
      setProvider: (name) => {
        switchProvider(name);
      },
      // changeDir: 直接调 api.ts 的 changeDir（POST /cd），并把新目录上报给 App。
      changeDir: async (path) => {
        const res = await changeDir(path);
        if (res.success) {
          onCwdChanged?.(res.current_dir);
          pushCommandNotice(t('cmd.cd.done', { dir: res.current_dir }));
        } else {
          pushCommandNotice(res.message);
        }
      },
      // openSessionSidebar: 侧栏开关状态在 App，Chat 无此 prop。
      openSessionSidebar: () => { pushCommandNotice(t('cmd.resume.openHint')); },
      // reloadConfig: POST /config/reload。
      reloadConfig: async () => {
        await postConfigReload();
        pushCommandNotice(t('cmd.reload.done'));
      },
      openSlashSkillsMenu: () => {
        // Open a pure SKILLS browser (not the full '/' command list — that's what
        // made running /skills look like it "reset to /"). Input stays '/', but the
        // menu shows only skills; typing anything drops back to normal filtering.
        // Trigger the skills fetch if it hasn't been loaded yet.
        setInput('/');
        setSlashQuery('');
        setSlashIndex(0);
        setSlashSkillsOnly(true);
        if (slashSkills === null && !slashLoading) {
          setSlashLoading(true);
          getSkills()
            .then(setSlashSkills)
            .catch(() => setSlashSkills([]))
            .finally(() => setSlashLoading(false));
        }
        setSlashOpen(true);
        requestAnimationFrame(() => {
          const ta = textareaRef.current;
          if (!ta) return;
          ta.focus();
          ta.setSelectionRange(1, 1);
          ta.style.height = 'auto';
          ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
        });
      },
      notice: (text) => pushCommandNotice(text),
      submitPrompt: (text) => {
        if (busyRef.current) {
          setQueued((q) => [
            ...q,
            {
              id: queueIdRef.current++,
              text,
              approvalMode: modeState.confirmedMode,
            },
          ]);
          return;
        }
        void deliver(text, [], modeState.confirmedMode);
      },
      execServerCommand: async (command, arg) => {
        const SESSION_MUTATING = new Set(['undo', 'compact']);
        if (SESSION_MUTATING.has(command)) {
          if (busyRef.current) { pushCommandNotice(t('cmd.session.busy')); return; }
          if (sync) {
            // sync 模式：compact 派发到共享实时运行时（结果经 /live 的 Warning 事件
            // 渲染压缩标记）；undo 暂无实时路径，维持拒绝。
            if (command === 'compact') {
              // Keep a handle on the pending notice so a failed dispatch can retract
              // it — otherwise a stale "Compacting…" line lingers next to the error.
              const pendingNote: Message = {
                role: 'system',
                parts: [{ kind: 'notice', text: t('cmd.compact.pending') }],
                ts: Date.now(),
              };
              setMessages((prev) => [...prev, pendingNote]);
              // Gate sendMessage while the dispatch is in flight, mirroring the
              // non-sync path (compactingRef guards the submit handler).
              compactingRef.current = true;
              try {
                const { accepted } = await postLiveCompact();
                if (!accepted) pushCommandNotice(t('cmd.compact.syncNoRuntime'));
              } catch (e) {
                setMessages((prev) => prev.filter((m) => m !== pendingNote));
                pushCommandNotice(t('chat.connError', { msg: e instanceof Error ? e.message : String(e) }));
              } finally {
                compactingRef.current = false;
              }
              return;
            }
            pushCommandNotice(t('cmd.session.syncUnsupported'));
            return;
          }
        }
        if (command === 'compact') pushCommandNotice(t('cmd.compact.pending'));
        const isCompact = command === 'compact';
        if (isCompact) compactingRef.current = true;
        try {
          let res: CommandResult;
          try {
            res = await postCommand({
              command,
              arg,
              session_id: sessionId ?? undefined,
              working_dir: cwd ?? undefined,
              project_hash: activeSession?.project_hash ?? undefined,
              provider: provider ?? undefined,
            });
          } catch (e) {
            pushCommandNotice(t('chat.connError', { msg: e instanceof Error ? e.message : String(e) }));
            return;
          }
          if (res.kind === 'error') { pushCommandNotice(res.message); return; }
          if (res.kind === 'undo') {
            if (res.undone > 0 && sessionId) await reloadSessionTranscript(sessionId);
            pushCommandNotice(res.undone > 0 ? t('cmd.undo.done', { n: res.undone }) : t('cmd.undo.none'));
            return;
          }
          if (res.kind === 'remember') { pushCommandNotice(t('cmd.remember.done', { scope: res.scope })); return; }
          if (res.kind === 'forget') { pushCommandNotice(t('cmd.forget.done', { n: res.removed.length })); return; }
          if (res.kind === 'memory') {
            const lines = [...res.global.map((e) => `[global] ${e}`), ...res.project.map((e) => `[project] ${e}`)];
            pushCommandNotice(lines.length ? `${t('cmd.memory.header')}\n${lines.join('\n')}` : t('cmd.memory.empty'));
            return;
          }
          if (res.kind === 'compact') {
            if (res.applied) {
              if (sessionId) await reloadSessionTranscript(sessionId);
              pushCommandNotice(t('cmd.compact.done', { n: res.removed_messages, before: res.before_tokens, after: res.after_tokens }));
            } else {
              pushCommandNotice(t('cmd.compact.none'));
            }
            return;
          }
          if (res.kind === 'context') {
            pushCommandNotice(
              t('cmd.context.body', {
                used: res.used_tokens, msgs: res.total_messages,
                pct: Math.round(res.utilization * 100),
                window: res.ctx_window, name: res.ctx_name,
              })
            );
            return;
          }
          if (res.kind === 'whoami') {
            pushCommandNotice(res.logged_in
              ? t('cmd.whoami.body', { name: res.name ?? res.username ?? '', user: res.username ?? '', email: res.email ?? '—' })
              : t('cmd.whoami.none'));
            return;
          }
          if (res.kind === 'status') {
            pushCommandNotice(res.text);
            return;
          }
          if (res.kind === 'config') { pushCommandNotice(t('cmd.config.body', { path: res.path, provider: res.provider || '—' })); return; }
          if (res.kind === 'diff') { pushCommandNotice(res.stat.trim() ? res.stat : t('cmd.diff.clean')); return; }
          if (res.kind === 'cost') { pushCommandNotice(t('cmd.cost.body', { tokens: res.total_tokens, turns: res.turn_count })); return; }
          if (res.kind === 'todo') {
            if (!res.items.length) { pushCommandNotice(t('cmd.todo.empty')); return; }
            const mark = (s: string) => (s === 'completed' ? '[x]' : s === 'in_progress' ? '[~]' : '[ ]');
            pushCommandNotice(`${t('cmd.todo.header')}\n${res.items.map((i) => `${mark(i.status)} ${i.content}`).join('\n')}`);
            return;
          }
        } finally {
          if (isCompact) compactingRef.current = false;
        }
      },
      t,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [t, modeState, sync, onCwdChanged, slashSkills, slashLoading, sessionId, cwd, activeSession, provider, messages.length],
  );

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
      case 'runtime_info':
        providerRef.current = event.provider;
        setProvider(event.provider);
        break;
      case 'session_assigned':
        // Bind a brand-new `/chat` conversation before model/provider work can
        // fail or the SSE transport can disappear. Mark the current canvas as
        // already loaded so the App state update cannot replace the optimistic
        // first turn with an empty history fetch.
        activeIdRef.current = event.session_id;
        loadedForRef.current = event.session_id;
        onSessionId(event.session_id);
        break;
      case 'command_output':
        pushCommandNotice(event.text);
        break;

      case 'text':
        appendToLastAssistant(event.content);
        break;

      case 'reasoning':
        // 推理内容当前不渲染（沿用既有行为），但首个推理事件即视为输出
        // 活动：清除三点 waiting，恢复流式光标。
        setMessages((prev) => {
          if (prev.length === 0) return prev;
          const last = prev[prev.length - 1];
          if (last.role !== 'assistant' || last.outputStarted) return prev;
          return [...prev.slice(0, -1), { ...last, outputStarted: true }];
        });
        break;

      case 'tool_start': {
        const argsStr = formatArgs(event.arguments);
        if (isTodoTool(event.name)) {
          pendingTodoCallsRef.current.set(event.id, { name: event.name, args: argsStr });
          stageTodoCall(event.id, event.name, argsStr);
        }
        addToolToLastAssistant({
          id: event.id,
          name: event.name,
          args: argsStr,
          detail: formatToolDetail(event.name, argsStr, todoTitlesRef.current),
          status: 'pending',
        });
        break;
      }

      case 'tool_output':
        appendToolOutput(event.chunk);
        break;

      case 'tool_progress':
        setMessages((prev) => {
          if (prev.length === 0) return prev;
          const last = prev[prev.length - 1];
          if (last.role !== 'assistant') return prev;
          return [
            ...prev.slice(0, -1),
            { ...last, parts: updateToolProgress(last.parts, event.id, event.progress) },
          ];
        });
        break;

      case 'tool_result':
        {
          const pending = pendingTodoCallsRef.current.get(event.id);
          if (pending) settleTodoCall(event.id, event.success);
          pendingTodoCallsRef.current.delete(event.id);
        }
        updateToolInLastAssistant(event.id, {
          status: toolResultStatus(event.success, event.output),
          duration_ms: event.duration_ms,
          output: event.output,
          progress: undefined,
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

      case 'user_input_request':
        setUserInputReq({ ...event, response_transport: 'chat' });
        break;

      case 'policy_intervention':
        setPolicyIntervention(event);
        break;

      case 'done': {
        // A normal terminal keeps only successful calls; a user cancellation
        // retires the active plan at the same boundary as kernel/TUI.
        stampLastAssistantCompletion();
        finishTodoTurn(event.stop_reason === 'cancelled');
        // 标记这是本 Chat 自己产生的会话 id，避免下面的 useEffect 误把当前对话清空，
        // 并标记其历史「已就位」（就是当前画布），防止 project_hash 回填后重新加载覆盖。
        activeIdRef.current = event.session_id;
        loadedForRef.current = event.session_id;
        if (event.stop_reason === 'cancelled') {
          todoCacheRef.current.set(event.session_id, []);
        }
        onSessionId(event.session_id);
        const terminal = classifyChatDone({
          stopReason: event.stop_reason,
          message: event.message,
        });
        if (terminal.discardQueued) {
          // An abnormal terminal preserves the partial transcript, but queued
          // input was composed under the assumption that this turn succeeded.
          // Never execute it automatically after an incomplete turn.
          setQueued([]);
          pushNoticeToLastAssistant(t('chat.incomplete', { msg: terminal.detail }));
        }
        transitionChatRecovery({ type: 'authoritative_terminal' });
        setBusy(false);
        setTurnStats(completedTurnStats(event.stats, event.stop_reason));
        onPermissionResolved?.(null); // 回合结束：兜底清掉任何残留审批卡片
        setUserInputReq(null);
        if (event.stop_reason !== 'policy_denied') setPolicyIntervention(null);
        finishTurnNotification({
          stopReason: event.stop_reason,
          sessionId: event.session_id,
          message: event.message,
          durationMs: event.stats?.duration_ms,
          dedupeKey: notificationDedupeKey(
            event.session_id,
            event.stop_reason,
            event.stats,
          ),
        });
        break;
      }

      case 'stopped':
        // User cancellation is an authoritative interruption boundary: the
        // previous plan remains historical but is no longer active.
        stampLastAssistantCompletion();
        resetTodoBatch(true);
        transitionChatRecovery({ type: 'authoritative_terminal' });
        setBusy(false);
        setQueued([]); // 用户中止：丢弃排队消息（对齐 VSCode 插件）
        onPermissionResolved?.(null);
        setUserInputReq(null);
        finishTurnNotification({
          stopReason: 'cancelled',
          sessionId: activeIdRef.current ?? undefined,
        });
        break;

      case 'error':
        stampLastAssistantCompletion();
        resetTodoBatch(false);
        appendToLastAssistant('\n\n' + t('chat.error', { msg: event.message }));
        transitionChatRecovery({ type: 'authoritative_terminal' });
        setBusy(false);
        setQueued([]); // 出错：丢弃排队消息
        onPermissionResolved?.(null);
        setUserInputReq(null);
        finishTurnNotification({
          stopReason: 'error',
          sessionId: activeIdRef.current ?? undefined,
          message: event.message,
        });
        break;

      case 'warning':
        // 非致命提示（如"已自动压缩上下文"）：渲染成淡色 notice 行 —— 不染红、不并进
        // 回复文本、不结束回合（任务继续）。对齐 TUI 的黄色 "!" 提示。
        pushNoticeToLastAssistant(t('chat.warning', { msg: event.message }));
        break;

      case 'persistence_warning':
        setPersistenceWarning(event.message);
        break;

      case 'rate_limited': {
        // 限流暂停：渲染成暗色中性卡片，非红色 error 样式；保留已完成内容，不结束回合。
        // auto_resuming=true → WaitAndRetry (kernel will sleep then retry)
        // A CodingPlan verdict carries window data (a reset time AND/OR a window
        // label); the kernel's generic default (an external-model / non-CodingPlan
        // 429) carries NEITHER. So only claim "5h window exhausted" for a real
        // CodingPlan quota — otherwise a generic "限流（HTTP 429）". Mirrors the TUI/CLI.
        const time = event.reset_at_display;
        const isCodingPlan = !!time || !!event.reset_label;
        const secs = event.secs_until_reset;
        // Bare compact h/m/s (locale-neutral), then wrap in a localized suffix so the
        // English notice doesn't leak a Chinese "（还有 …）" fragment.
        const durRaw = secs == null ? '' :
          secs >= 3600 ? `${Math.floor(secs / 3600)}h${Math.floor((secs % 3600) / 60)}m` :
          secs >= 60 ? `${Math.floor(secs / 60)}m` : `${secs}s`;
        const dur = durRaw ? t('chat.rateLimited.remaining', { dur: durRaw }) : '';
        let text: string;
        if (event.auto_resuming) {
          text = t('chat.rateLimited.waiting', { secs: String(event.secs_until_reset ?? 0) });
        } else if (!isCodingPlan) {
          // Generic 429 (external model / no CodingPlan window data): must NOT be
          // dressed up as a CodingPlan quota exhaustion. Surface the provider's OWN
          // reason when present (e.g. an external model's "余额不足…请充值").
          const reason = event.server_message?.trim() ? `：${event.server_message.trim()}` : '';
          text = `${t('chat.rateLimited.generic')}${reason}${dur} · ${t('chat.rateLimited.hint')}`;
        } else if (time) {
          text = `${t('chat.rateLimited.paused', { time })} · ${t('chat.rateLimited.hint')}`;
        } else {
          // CodingPlan window (label present) with no wall-clock reset time.
          text = `${t('chat.rateLimited.pausedNoTime')}${dur} · ${t('chat.rateLimited.hint')}`;
        }
        pushRateLimitedToLastAssistant(text);
        break;
      }

      // Artifact events: the daemon's ArtifactDetector strips fenced code blocks
      // (and HTML/SVG) from TextDelta and emits them as separate artifact_* events.
      // Without handling these, the code content is silently lost in the WebUI.
      case 'artifact_start': {
        artifactLangRef.current = event.language ?? '';
        artifactBufRef.current = '';
        break;
      }
      case 'artifact_content': {
        artifactBufRef.current += event.content;
        break;
      }
      case 'artifact_end': {
        const lang = artifactLangRef.current;
        const code = artifactBufRef.current;
        artifactLangRef.current = '';
        artifactBufRef.current = '';
        // Reconstruct the fenced code block so the Markdown renderer can process it.
        // The daemon stripped the ``` fences; we put them back here.
        const fence = '```';
        const tag = lang ? fence + lang : fence;
        // The detector's body already ends with the newline before the closing
        // fence (find_code_fence_end returns line_start), so only add one if it's
        // missing — otherwise the block renders with a spurious trailing blank line.
        const body = code.endsWith('\n') ? code : code + '\n';
        appendToLastAssistant('\n' + tag + '\n' + body + fence + '\n');
        break;
      }

      default:
        // Ignore tool_batch, etc.
        break;
    }
  }

  // 实际投递一条消息（同步 / 常规两条路径）；busy 由各自的事件流复位。
  async function deliver(
    text: string,
    images: ImageData[],
    approvalMode: ApprovalMode = modeState.confirmedMode,
  ): Promise<boolean> {
    if (!chatRecoveryPolicy(chatRecoveryRef.current).allowSend) {
      pushCommandNotice(t('chat.recoveryBlocked'));
      return false;
    }
    // Actually sending a message (immediate OR drained from the queue) re-engages
    // auto-follow — the user wants to see their message + the reply. Placed HERE, not in
    // sendMessage, so merely QUEUEING a message while reading history doesn't yank them.
    atBottomRef.current = true;
    setShowJumpBtn(false);
    const startsNewTurn = !busyRef.current;
    if (startsNewTurn) {
      setTodoPanelVisible(reduceTodoPanelVisibility(
        todoPanelVisibleRef.current,
        { type: 'user_input' },
      ));
    }
    // 本会话首条消息：用消息前 10 字做临时标题，立刻通知 App 乐观插入侧栏，
    // 让会话「一发送就出现在左侧」。回合 done 后列表刷新会换成后端自动命名。
    if (!optimisticFiredRef.current && messages.length === 0) {
      optimisticFiredRef.current = true;
      const title = (text.split('\n')[0]?.trim() ?? '').slice(0, 10);
      if (title) onOptimisticSession?.(title);
    }

    if (sync) {
      // ── Sync path: send to /live/message. Optimistically append the user
      //    message NOW so the view leaves the "new conversation" landing page
      //    immediately (the server `user` echo — which keeps OTHER tabs in sync —
      //    only arrives after VL preprocessing, so waiting for it leaves the
      //    sender stuck on the empty page). Our own echo is deduped in the `user`
      //    case via `pendingSelfEchoRef`.
      const steering = busyRef.current;
      setBusy(true);
      // 回合起点只记录一次（steer 不重置）：供终态通知计算 durationMs。
      if (!steering) {
        notificationTurnActiveRef.current = true;
        turnStartedAtRef.current = Date.now();
      }
      const now = Date.now();
      const pendingSteer: PendingLiveSteer = {
        id: crypto.randomUUID(),
        text,
        images: images.length ? images : undefined,
        confirmed: false,
      };
      pendingSelfEchoRef.current.push({ id: pendingSteer.id, text });
      setMessages((prev) => [
        ...prev,
        {
          role: 'user',
          parts: [{ kind: 'text', text }],
          images: images.length ? images : undefined,
          ts: now,
          pendingSteerId: pendingSteer.id,
        },
        { role: 'assistant', parts: [], pendingSteerId: pendingSteer.id },
      ]);
      // Register before the HTTP round-trip. A very fast round boundary can
      // emit `steered` on SSE before the submit response reaches this tab.
      setPendingSteers((pending) => [...pending, pendingSteer]);
      try {
        const submittedProvider = provider;
        const receipt = await postLiveMessage(
          text,
          images.length ? images : undefined,
          provider ?? undefined,
          activeIdRef.current,
          pendingSteer.id,
        );
        if (shouldApplySteerProviderFallback(
          submittedProvider,
          providerRef.current,
          receipt.providerChangeApplied,
          receipt.provider,
        )) {
          // A mid-turn message is a steer and cannot reload the runtime provider.
          // Follow the daemon's authoritative provider instead of leaving the
          // selector pinned to a model that never became active.
          providerRef.current = receipt.provider;
          setProvider(receipt.provider);
          providerPinnedRef.current = false;
          pushCommandNotice(t('cmd.model.syncBusy'));
        }
        // The receipt is authoritative. Reconcile from the lifecycle's observed
        // terminal — NOT the transient `running` flag, which is false for a tab
        // that attached mid-turn. A `steered` submit is never bounced back to the
        // composer: the runtime folds it, or (if the turn already ended) re-runs
        // it as the next turn, matching the TUI.
        const outcome = reconcileSteerReceipt(receipt.disposition, liveLifecycleRef.current);
        if (outcome === 'confirm') {
          setPendingSteers((pending) => pending.map((item) => (
            item.id === pendingSteer.id ? { ...item, confirmed: true } : item
          )));
        } else {
          // 'clear': a new turn began; the submit IS that turn's input.
          // 'release': the steer raced the turn terminal; the runtime re-runs it
          //   as the next turn, so drop the marker and defer to that re-run.
          setPendingSteers((pending) => pending.filter((item) => item.id !== pendingSteer.id));
        }
      } catch (error) {
        // Roll back the optimistic append — the send never reached the server.
        const echoIndex = pendingSelfEchoRef.current.findIndex(
          (pending) => pending.id === pendingSteer.id,
        );
        if (echoIndex >= 0) pendingSelfEchoRef.current.splice(echoIndex, 1);
        setMessages((prev) => prev.filter((message) => message.pendingSteerId !== pendingSteer.id));
        setPendingSteers((pending) => pending.filter((item) => item.id !== pendingSteer.id));
        if (steering) {
          setInput((current) => [text, current].filter(Boolean).join('\n'));
          setPendingImages((current) => [...images, ...current]);
        } else {
          setBusy(false);
          notificationTurnActiveRef.current = false;
          turnStartedAtRef.current = null;
        }
        setQueued([]);
        setHistoryHint(t('chat.connError', { msg: String(error) }));
        return false;
      }
      // 消息发出后延迟刷新侧栏列表，给后端落盘时间；
      // turn 完成后 state(running=false) 会再刷一次确保更新。
      setTimeout(() => onLiveTurnDone?.(), 200);
      return true;
    }

    // ── Normal path ──
    setBusy(true);
    // 回合起点只记录一次：供终态通知计算 durationMs。
    notificationTurnActiveRef.current = true;
    turnStartedAtRef.current = Date.now();
    // 消息发出后延迟刷新侧栏列表，给后端落盘时间；
    // done 事件中 onSessionId 会再刷一次确保更新。
    setTimeout(() => onLiveTurnDone?.(), 200);

    // Push user message + empty assistant placeholder
    const now = Date.now();
    setMessages((prev) => [
      ...prev,
      { role: 'user', parts: [{ kind: 'text', text }], images: images.length ? images : undefined, ts: now },
      { role: 'assistant', parts: [] },
    ]);

    const controller = new AbortController();
    abortRef.current = controller;
    const requestId = crypto.randomUUID();
    requestIdRef.current = requestId;
    const requestGeneration = sessionGenerationRef.current;
    let keepStopAlias = false;

    try {
      const body = {
        message: text,
        ...(sessionId ? { session_id: sessionId } : {}),
        request_id: requestId,
        ...(cwd ? { working_dir: cwd } : {}),
        ...(provider ? { provider } : {}),
        ...(images.length ? { images } : {}),
        approval_mode: approvalMode,
      };

      await streamChat(
        body,
        (event) => {
          if (
            isCurrentChatStream(
              requestId,
              requestGeneration,
              requestIdRef.current,
              sessionGenerationRef.current,
              controller.signal.aborted,
            )
          ) {
            handleEvent(event);
          }
        },
        controller.signal,
      );
    } catch (err: unknown) {
      const stillCurrent = isCurrentChatStream(
        requestId,
        requestGeneration,
        requestIdRef.current,
        sessionGenerationRef.current,
        controller.signal.aborted,
      );
      const aborted = err instanceof Error && err.name === 'AbortError';
      if (!aborted && stillCurrent) {
        // Transport loss is not a turn terminal. Keep the request alias so the
        // user can retry the existing stop protocol, but release the visual
        // busy state and prohibit sends/queue drain until recovery is explicit.
        keepStopAlias = true;
        transitionChatRecovery({ type: 'transport_lost' });
        const msg = err instanceof Error ? err.message : String(err);
        appendToLastAssistant('\n\n' + t('chat.connError', { msg }));
      }
      if (stillCurrent) {
        setBusy(false);
        setQueued([]); // 连接错误：与 stopped/error 一致，丢弃排队消息
        // 中止/连接错误时流被掐断，不会再有 done/stopped 事件 → 兜底清掉审批卡片，
        // 否则点「停止」时若正挂着审批卡片，它会一直残留。
        onPermissionResolved?.(null);
      }
    } finally {
      if (abortRef.current === controller) abortRef.current = null;
      if (
        requestIdRef.current === requestId &&
        sessionGenerationRef.current === requestGeneration &&
        !keepStopAlias
      ) requestIdRef.current = null;
    }
    return !keepStopAlias;
  }

  function sendMessage() {
    const text = input.trim();
    const images = pendingImages;
    if (!text && images.length === 0) return;

    // 斜杠命令拦截：命中已知命令则执行且不作为聊天发送。带图时不拦截（命令不处理图片）。
    if (images.length === 0) {
      const parsed = parseSlashCommand(text);
      if (parsed && slashCommandMap.has(parsed.name)) {
        // Preserve the existing command gate: this fix only makes ordinary
        // prompts queueable while a turn is active.
        if (modeState.pendingMode || compactingRef.current) return;
        setInput('');
        if (textareaRef.current) textareaRef.current.style.height = 'auto';
        setSlashOpen(false);
        setHistoryHint(null);
        void dispatchSlashCommand(text, slashCommandMap, slashHandlers)
          .catch((e) => pushCommandNotice(t('chat.connError', { msg: e instanceof Error ? e.message : String(e) })));
        return;
      }
    }

    // Active turns accept ordinary input before transient mode/compaction
    // gates: /chat queues it for the next turn and /live treats it as a steer.
    // Those gates only prevent starting a new idle turn.
    const disposition = activeTurnSubmissionDisposition(
      busyRef.current,
      syncRef.current,
      Boolean(modeState.pendingMode),
      compactingRef.current,
    );
    if (disposition === 'blocked') return;

    if (!chatRecoveryPolicy(chatRecoveryRef.current).allowSend) {
      pushCommandNotice(t('chat.recoveryBlocked'));
      return;
    }

    // 清空输入框（无论立即发送还是排队）。
    setInput('');
    setPendingImages([]);
    // 重置输入框高度：清空 value 不会复位之前 auto-resize 撑高的内联 height
    if (textareaRef.current) textareaRef.current.style.height = 'auto';
    setHistoryHint(null);

    // Read the synchronous projection, not the render closure: an SSE state
    // transition and a click/Enter can land before Preact commits a frame.
    if (disposition === 'queue') {
      setQueued((q) => [
        ...q,
        {
          id: queueIdRef.current++,
          text,
          images: images.length ? images : undefined,
          approvalMode: modeForQueuedPrompt(modeState),
        },
      ]);
      return;
    }

    // `deliver` itself distinguishes a live steer from a new live turn.
    // For an idle turn this is an ordinary send; for active `/live` it is steer.
    void deliver(text, images);
  }

  // 当前回合结束(done)后，依次发送排队消息；stopped/error/连接错误已清空队列。
  useEffect(() => {
    if (
      busy ||
      queued.length === 0 ||
      modeState.pendingMode ||
      !chatRecoveryPolicy(chatRecoveryRef.current).allowQueueDrain
    ) return;
    const next = queued[0];
    setQueued((q) => q.slice(1));
    void deliver(next.text, next.images ?? [], next.approvalMode);
    // deliver 为组件内函数声明，闭包始终取最新渲染值；仅以 busy/queued 触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busy, queued, modeState.pendingMode, chatRecovery]);

  function handleKeyDown(e: KeyboardEvent) {
    // Safari/WebKit and a few desktop IMEs can report keyCode=229 while
    // `isComposing` is false. Never let menu navigation or Enter-to-send steal
    // a key that still belongs to the IME candidate window.
    if (composingRef.current || e.isComposing || e.keyCode === 229) return;

    // 斜杠菜单导航（使用与渲染层一致的合并列表：本地命令 + 远程技能）
    if (slashOpen) {
      const mergedItems = buildSlashMenuItems(FRONTEND_COMMANDS, slashSkills ?? [], slashQuery, t as (k: string) => string, slashSkillsOnly);
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSlashIndex((i) => Math.min(i + 1, mergedItems.length - 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSlashIndex((i) => Math.max(i - 1, 0));
        return;
      }
      if (e.key === 'Enter' && mergedItems.length > 0) {
        e.preventDefault();
        insertSkill(mergedItems[slashIndex].name);
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

    if (shouldSendComposerOnEnter(e, coarsePointer)) {
      e.preventDefault();
      sendMessage();
    }
  }

  async function handleStop() {
    try {
      const recoveryNeedsStop = chatRecoveryPolicy(
        chatRecoveryRef.current,
      ).allowStop;
      if (requestIdRef.current && (!sync || recoveryNeedsStop)) {
        const requestAlias = requestIdRef.current;
        const detached = abortRef.current === null;
        await stopChat(requestAlias);
        if (detached && requestIdRef.current === requestAlias) {
          transitionChatRecovery({ type: 'stop_succeeded' });
          requestIdRef.current = null;
          if (!sync) setBusy(false);
          setQueued([]);
          onPermissionResolved?.(null);
          pushCommandNotice(t('chat.detachedStopped'));
        }
      } else if (sync) {
        await postLiveStop();
      }
    } catch (error) {
      setQueued([]);
      pushNoticeToLastAssistant(t('chat.cancelFailed', { error: String(error) }));
      if (!sync) {
        // Keep an attached stream alive so a later authoritative terminal can
        // still recover it. A detached stream has no reattach path, so it stays
        // explicitly locked with its stop alias available for retry.
        transitionChatRecovery({ type: 'stop_failed' });
        if (abortRef.current === null) setBusy(false);
      }
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

  // Auto-resize textarea + slash-command + @-mention detection. Runs on every
  // input event (composition drafts included) so state always mirrors the DOM;
  // Preact's equal-value check then never writes the pre-edit buffer back.
  function commitComposerInput(ta: HTMLTextAreaElement) {
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
        // Any real keystroke exits the pure-skills browser → normal mixed filtering.
        setSlashSkillsOnly(false);
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

  function handleInput(e: Event) {
    const ta = e.target as HTMLTextAreaElement;
    ta.style.height = 'auto';
    ta.style.height = Math.min(ta.scrollHeight, 160) + 'px';
    // 始终把 DOM 值镜像进 state（包括组合期的中间草稿）。textarea 保持受控
    // (value={input})，而 Preact 只在 state 与 DOM 值不同时才写回，因此：
    //   - 组合期 state 永远等于 DOM，写回不会发生，IME 预编辑缓冲不受干扰；
    //   - 无关 re-render 也不会把陈旧值写回（旧的 `value={composing ? undefined
    //     : input}` 失控 hack 会在 compositionend 读到尚未提交进 ta.value 的
    //     旧值，随后受控渲染把它强写回，刚选中的中文整段被抹掉——即"再输入
    //     会覆盖原文"的 bug）。
    commitComposerInput(ta);
  }

  function handleCompositionStart() {
    composingRef.current = true;
  }

  function handleCompositionEnd() {
    composingRef.current = false;
    // 不在此处提交值：compositionend 之后浏览器必定补发一次 input 事件
    // （inputType=insertCompositionText，携带最终值），由 handleInput 同步。
    // 在 compositionend 直接读 ta.value 在 WebKit 下可能还是预编辑旧值。
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

  // 会话内搜索: Cmd/Ctrl+F 呼出浮动搜索框,Esc/× 关闭;输入关键词改变字符背景并高亮,
  // Enter/Shift+Enter(或 ↑/↓)在匹配消息间跳转并滚动定位(反查定位)。关闭态
  // 完全不占布局空间,不挤压消息区。仅前端,不动后端。
  const [search, setSearch] = useState('');
  const [searchOpen, setSearchOpen] = useState(false);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const searchTrim = search.trim().toLowerCase();
  // 不过滤时间线，而是展示完整消息流
  const visibleMessages = useMemo(() => messages.map((m, origIdx) => ({ msg: m, origIdx })), [messages]);
  const lastVisibleIdx = visibleMessages.length - 1;
  // 计算匹配的原始消息索引列表
  const matchPositions = useMemo(() => {
    if (!searchOpen || !searchTrim) return [];
    const positions: number[] = [];
    messages.forEach((m, idx) => {
      if (messageText(m).toLowerCase().includes(searchTrim)) {
        positions.push(idx);
      }
    });
    return positions;
  }, [messages, searchTrim, searchOpen]);
  // 匹配消息 origIdx → DOM 节点,供 ↑/↓/Enter 滚动定位。每次渲染重填。
  const matchRefs = useRef<Record<number, HTMLElement | null>>({});
  const [matchIdx, setMatchIdxState] = useState(0);
  // P3 修复: 用 ref 镜像 matchIdx,让事件处理器在快速连击时永远读到最新值
  const matchIdxRef = useRef(0);
  const setMatchIdx = (v: number) => { matchIdxRef.current = v; setMatchIdxState(v); };
  const closeSearch = () => { setSearch(''); setMatchIdx(0); setSearchOpen(false); };
  // 搜索导航 helper: 算 newIdx → setMatchIdx → 滚动。
  const navMatch = (delta: number) => {
    const n = matchPositions.length;
    if (n === 0) return;
    const newIdx = (matchIdxRef.current + delta + n) % n;
    setMatchIdx(newIdx);
    const targetOrigIdx = matchPositions[newIdx];
    const node = matchRefs.current[targetOrigIdx];
    if (node) node.scrollIntoView({ behavior: 'smooth', block: 'center' });
  };

  // Cmd/Ctrl+F 打开搜索并聚焦;Esc 关闭。绑定在 window 层级,焦点在输入框/按钮/
  // 消息气泡时都生效;阻止浏览器默认查找以免误触原生 UI。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === 'f') {
        if (messages.length === 0) return;
        e.preventDefault();
        setSearchOpen(true);
        // 下一帧再聚焦,等 input 挂载。
        requestAnimationFrame(() => searchInputRef.current?.focus());
      } else if (e.key === 'Escape' && searchOpen) {
        e.preventDefault();
        closeSearch();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searchOpen, messages.length]);

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
              // stopPropagation: this mousedown is an INSIDE click. Without it the
              // event bubbles to the document click-outside handler, whose
              // `contains(target)` guard fails once `setAtMention` clears+refetches
              // the list (the clicked row detaches), so it wrongly closes the menu —
              // a directory then can't drill in via mouse (keyboard was immune since
              // it never triggers a document mousedown). preventDefault keeps focus.
              onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); chooseAtRow(item); }}
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
          {buildSlashMenuItems(FRONTEND_COMMANDS, slashSkills ?? [], slashQuery, t as (k: string) => string, slashSkillsOnly).map((item, i) => (
            <button
              key={`${item.kind}:${item.name}`}
              class={'slash-row' + (i === slashIndex ? ' active' : '')}
              // stopPropagation for the same reason as the @-menu rows above: keep
              // this inside-click from reaching the document click-outside handler.
              onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); insertSkill(item.name); }}
              onMouseEnter={() => setSlashIndex(i)}
              type="button"
              title={item.description || ''}
            >
              <span class="slash-name">/{item.name}</span>
              {item.description && <span class="slash-desc">{item.description}</span>}
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
        enterkeyhint={coarsePointer ? 'enter' : 'send'}
        onInput={handleInput}
        onKeyDown={handleKeyDown}
        onCompositionStart={handleCompositionStart}
        onCompositionEnd={handleCompositionEnd}
        onPaste={handlePaste}
      />
      <div class="input-footer">
        <div class="input-footer-primary">
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
        </div>
        <div class="input-footer-actions">
          <ModeSelector
            value={modeState.displayMode}
            disabled={Boolean(modeState.pendingMode)}
            onChange={(m) => switchMode(m)}
          />
          <ModelSelector
            value={provider}
            liveEffort={reasoningEffort?.provider === provider ? reasoningEffort.effort : undefined}
            onChange={(p) => switchProvider(p)}
            onDefaultChange={followDefaultProvider}
          />
          <div class="input-turn-controls">
            {busy || recoveryPolicy.allowStop ? (
              <>
                {/* /live folds this into the active turn; /chat queues it for the next turn. */}
                {recoveryPolicy.allowSend && (input.trim() || pendingImages.length > 0) && (
                  <button
                    class="btn-send"
                    onClick={sendMessage}
                    title={sync ? t('chat.steer') : t('chat.queue')}
                    aria-label={sync ? t('chat.steer') : t('chat.queue')}
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
                disabled={!recoveryPolicy.allowSend || Boolean(modeState.pendingMode) || (!input.trim() && pendingImages.length === 0)}
                title={recoveryPolicy.allowSend ? t('chat.send') : t('chat.recoveryBlocked')}
                aria-label={recoveryPolicy.allowSend ? t('chat.send') : t('chat.recoveryBlocked')}
              >
                ↑
              </button>
            )}
          </div>
        </div>
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
      <span class="input-hint">
        {t(coarsePointer ? 'chat.kbdHintMobile' : 'chat.kbdHint')}
      </span>
    </div>
  );

  const cacheHit = turnStats ? turnCacheHit(turnStats) : null;
  const turnStatsText = turnStats ? [
    t('stats.counts', { rounds: turnStats.rounds, tools: turnStats.tool_calls }),
    t('stats.duration', { duration: formatTurnDuration(turnStats.duration_ms) }),
    cacheHit !== null ? t('stats.cacheHit', { percent: cacheHit }) : null,
    t('stats.tokens', {
      input: formatTurnTokens(turnStats.prompt_tokens),
      output: formatTurnTokens(turnStats.completion_tokens),
    }),
  ].filter((part): part is string => part !== null).join(' · ') : null;
  const turnStatsLine = turnStats && !busy && (
    <div class="turn-stats" role="status">
      {turnStatsText}
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

  // `/chat` owns its pending approval in App, but presentation belongs to the
  // same composer seat as `/live` interactions.
  const chatPermissionCard = pendingPermission && (
    <PermissionCard
      req={pendingPermission}
      onDone={() => onPermissionResolved?.(pendingPermission.call_id)}
    />
  );

  // Shared structured-input card for `/chat` and `/live`.
  const userInputCard = userInputReq && (
    <UserInputCard
      req={userInputReq}
      onDone={() => setUserInputReq(null)}
      submitAnswer={(body) => postUserInputAnswer(userInputReq, body)}
    />
  );

  // The kernel event precedes the authoritative terminal by design. Keep the
  // Keep the card mounted while its control-plane acknowledgement is pending so
  // loading/error state can render. Gate the action — not mounting — on `busy`:
  // the runtime accepts a resolution only after the interrupted turn is idle.
  const policyInterventionCard = policyIntervention && (
    <PolicyInterventionCard
      intervention={policyIntervention}
      busy={busy}
      onClose={() => setPolicyIntervention(null)}
      onResolve={async (action) => {
        if (sync) {
          await postLivePolicyInterventionResolution(policyIntervention.intervention_id, action);
        }
        pushCommandNotice(t(
          action === 'complete_externally'
            ? 'policyRecovery.completedLocally'
            : action === 'skip_step'
              ? 'policyRecovery.skippedLocally'
              : 'policyRecovery.endedLocally',
        ));
        return true;
      }}
    />
  );

  // A runtime should expose only one pending human decision. This ordering is
  // defensive against stale cross-channel UI state and keeps recovery actions
  // reachable before approvals and questions.
  const blockingInteraction = policyInterventionCard
    ?? livePermissionCard
    ?? chatPermissionCard
    ?? userInputCard;
  const hasBlockingInteraction = blockingInteraction != null;
  const wasBlockingInteractionRef = useRef(false);

  useEffect(() => {
    const wasBlocking = wasBlockingInteractionRef.current;
    wasBlockingInteractionRef.current = hasBlockingInteraction;
    if (!wasBlocking || hasBlockingInteraction) return;

    const frame = requestAnimationFrame(() => {
      textareaRef.current?.focus();
    });
    return () => cancelAnimationFrame(frame);
  }, [hasBlockingInteraction]);

  // 落地页快捷提示胶囊：点击把文本填入输入框并聚焦（不自动发送，便于二次编辑）。
  const quickChips: { label: string; insert: string }[] = [
    { label: t('chat.chipReview'), insert: '/review ' },
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
              {blockingInteraction ? (
                <div class="interaction-dock-seat">{blockingInteraction}</div>
              ) : (
                <>
                  {inputBox}
                  {inputSubbar}
                </>
              )}
            </div>
            {!blockingInteraction && (
              <div class="landing-chips">
                {quickChips.map((c) => (
                  <button key={c.label} class="landing-chip" onClick={() => fillInput(c.insert)}>
                    {c.label}
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>
        {filePickerModal}
      </>
    );
  }

  return (
    <>
      {/* Message timeline */}
      <div class="messages-container" ref={scrollRef} onScroll={recomputeAtBottom}>
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
          // system messages are transparent — they don't end an assistant turn.
          const turnTexts = new Map<number, string>();
          {
            let start = -1;
            let parts: string[] = [];
            for (let i = 0; i < messages.length; i++) {
              if (messages[i].role === 'assistant') {
                if (start < 0) start = i;
                const t = messageFullText(messages[i]);
                if (t) parts.push(t);
              } else if (messages[i].role === 'user') {
                // Only a user message ends the current assistant turn.
                if (start >= 0) {
                  for (let j = start; j < i; j++) {
                    turnTexts.set(j, parts.join('\n\n'));
                  }
                  start = -1;
                  parts = [];
                }
              }
              // role === 'system': transparent — do not flush the turn.
            }
            // Flush the last turn
            if (start >= 0) {
              for (let j = start; j < messages.length; j++) {
                turnTexts.set(j, parts.join('\n\n'));
              }
            }
          }

          const assistantTurnEnds = assistantTurnEndFlags(messages.map((message) => message.role));
          const turnArtifacts = artifactsByAssistantIndex(messages);

          return visibleMessages.map(({ msg, origIdx }, idx) => {
            const isLast = idx === lastVisibleIdx;
            const setMatchRef = (el: HTMLElement | null) => { matchRefs.current[origIdx] = el; };
            const timeLabel = formatMsgTime(msg.ts);
            const isActiveSearchMatch = searchOpen && searchTrim ? (origIdx === matchPositions[matchIdx]) : false;

            if (msg.role === 'user') {
              return (
                <UserMessageView
                  key={origIdx}
                  msg={msg}
                  searchRef={setMatchRef}
                  timeLabel={timeLabel}
                  search={search}
                  isActiveSearchMatch={isActiveSearchMatch}
                  steerPending={isSteerPending(msg.pendingSteerId, pendingSteers)}
                />
              );
            }

            // system messages render as a standalone notice row (no copy button,
            // no turn grouping, no streaming cursor).
            if (msg.role === 'system') {
              const fullText = msg.parts.map((p) => (p.kind === 'notice' ? p.text : '')).join('');
              const sysCls = 'msg-notice command-output' + (isActiveSearchMatch ? ' is-active-search-match' : '');
              return (
                <div class={sysCls} key={origIdx} ref={setMatchRef}>
                  {highlightText(fullText, search)}
                </div>
              );
            }

            // One user turn → one terminal assistant row. Compute this once over
            // the complete message sequence so intermediate rounds cannot each render
            // copy/time controls and system notices remain transparent.
            const isLastInTurn = assistantTurnEnds[origIdx] === true;

            return (
              <AssistantMessageView
                key={origIdx}
                msg={msg}
                isLast={isLast}
                busy={busy}
                lastIdx={lastIdx}
                isLastInTurn={isLastInTurn}
                turnText={turnTexts.get(origIdx) ?? ''}
                artifacts={turnArtifacts.get(origIdx) ?? []}
                sessionId={sessionId}
                searchRef={setMatchRef}
                timeLabel={timeLabel}
                search={search}
                isActiveSearchMatch={isActiveSearchMatch}
              />
            );
          });
        })()}

        {/* 排队中的消息：执行中输入、待当前回合结束后自动发送，可点 × 撤回。 */}
        {queued.map((q) => (
          <div
            key={`q-${q.id}`}
            class="user-message-wrapper queued"
            role="status"
            aria-label={t('chat.queued')}
          >
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

        {pendingSteers.some((item) => item.confirmed) && (
          <div class="steer-pending-status" role="status">
            {t('chat.steerPending', {
              count: pendingSteers.filter((item) => item.confirmed).length,
            })}
          </div>
        )}

        <div ref={bottomRef} />
        </div>
        {/* Zero-height sticky seat, following Harness: the control belongs to the
            conversation scroller and is centered on the same content lane. */}
        {showJumpBtn && (
          <div class="jump-to-bottom-slot">
            <button
              class="jump-to-bottom"
              onClick={() => scrollToBottom('smooth')}
              title={t('chat.jumpToBottom')}
              aria-label={t('chat.jumpToBottom')}
            >
              ↓
            </button>
          </div>
        )}
      </div>

      {/* 浮动搜索框:默认隐藏,Cmd/Ctrl+F 呼出,Esc/× 关闭。仿浏览器 Find-in-page 样式:
          长条胶囊、无图标、右侧依次 ↑ ↓ ×。position:absolute 钉在容器右上角,不占布局空间。
          Enter/↓ 下一条,Shift+Enter/↑ 上一条;零匹配时计数显示提示,导航按钮隐藏。 */}
      {searchOpen && messages.length > 0 && (
        <div class="msg-search-float" role="search" aria-label={t('chat.searchPlaceholder')}>
          <input
            class="msg-search-input"
            // type="text" 而非 "search"——后者在 Firefox 仍显示默认清除按钮,
            // 与自定义 × 重复。type="text" 全浏览器一致,清除仅走自定义路径。
            type="text"
            ref={searchInputRef}
            value={search}
            placeholder={t('chat.searchPlaceholder')}
            onInput={(e) => { setSearch((e.target as HTMLInputElement).value); setMatchIdx(0); }}
            onKeyDown={(e) => {
              // Enter/↓ = 下一条;Shift+Enter/↑ = 上一条。输入框内 ↑/↓ 不移动光标
              // (单行输入无光标位置歧义),直接在匹配间跳转。
              if (e.key === 'Enter') {
                if (searchTrim && matchPositions.length > 0) {
                  e.preventDefault();
                  navMatch(e.shiftKey ? -1 : 1);
                }
              } else if (e.key === 'ArrowDown' && searchTrim && matchPositions.length > 0) {
                e.preventDefault();
                navMatch(1);
              } else if (e.key === 'ArrowUp' && searchTrim && matchPositions.length > 0) {
                e.preventDefault();
                navMatch(-1);
              }
            }}
            aria-label={t('chat.searchPlaceholder')}
          />
          <span class="msg-search-count">
            {searchTrim
              ? (matchPositions.length > 0
                  ? `${matchIdx + 1}/${matchPositions.length}`
                  : t('chat.searchNoMatch'))
              : ''}
          </span>
          <button
            class="msg-search-nav"
            onClick={() => navMatch(-1)}
            title={t('chat.searchPrev')}
            aria-label={t('chat.searchPrev')}
            type="button"
            disabled={!searchTrim || matchPositions.length === 0}
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <line x1="12" y1="19" x2="12" y2="5" />
              <polyline points="5 12 12 5 19 12" />
            </svg>
          </button>
          <button
            class="msg-search-nav"
            onClick={() => navMatch(1)}
            title={t('chat.searchNext')}
            aria-label={t('chat.searchNext')}
            type="button"
            disabled={!searchTrim || matchPositions.length === 0}
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <line x1="12" y1="5" x2="12" y2="19" />
              <polyline points="19 12 12 19 5 12" />
            </svg>
          </button>
          <button
            class="msg-search-close"
            onClick={closeSearch}
            title={t('chat.searchClose')}
            aria-label={t('chat.searchClose')}
            type="button"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <line x1="18" y1="6" x2="6" y2="18" />
              <line x1="6" y1="6" x2="18" y2="18" />
            </svg>
          </button>
        </div>
      )}

      {/* Floating input */}
      {persistenceWarning && (
        <div class="persistence-warning" role="status">
          <span aria-hidden="true">⚠</span>
          <span>{persistenceWarning}</span>
          <button
            type="button"
            class="persistence-warning-dismiss"
            aria-label="Dismiss"
            onClick={() => setPersistenceWarning(null)}
          >
            ×
          </button>
        </div>
      )}
      <div class="input-container">
        {todoPanelVisible && (todoPreview ?? todoItems).length > 0 && (
          <TodoProgressPanel items={todoPreview ?? todoItems} />
        )}
        <div class="input-wrap">
          {blockingInteraction ? (
            <div class="interaction-dock-seat">{blockingInteraction}</div>
          ) : (
            <>
              {inputBox}
              {inputSubbar}
              {turnStatsLine}
            </>
          )}
        </div>
      </div>
      {filePickerModal}
    </>
  );
}

function TodoProgressPanel({ items }: { items: TodoItem[] }) {
  const t = useT();
  const [expanded, setExpanded] = useState(false);
  const inProgress = items.filter((item) => item.status === 'in_progress').length;
  const pending = items.filter((item) => item.status === 'pending').length;
  const completed = items.filter((item) => item.status === 'completed').length;
  const summary = [
    inProgress > 0 ? t('todo.panel.inProgress', { n: inProgress }) : '',
    pending > 0 ? t('todo.panel.pending', { n: pending }) : '',
    completed > 0 ? t('todo.panel.completed', { n: completed }) : '',
  ].filter(Boolean).join(' · ');

  return (
    <section class={'todo-progress-panel' + (expanded ? ' expanded' : '')} aria-label={t('todo.panel.title')}>
      <button
        type="button"
        class="todo-progress-head"
        aria-expanded={expanded}
        aria-label={expanded ? t('todo.panel.collapse') : t('todo.panel.expand')}
        onClick={() => setExpanded((value) => !value)}
      >
        <svg class="todo-progress-list-icon" width="18" height="18" viewBox="0 0 18 18" fill="none" aria-hidden="true">
          <circle cx="4" cy="4" r="1.5" stroke="currentColor" stroke-width="1.4" />
          <path d="m3.2 4 0.6.7L5 3.2M8 4h7M3 9h2M8 9h7M3 14h2M8 14h7" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
        <span class="todo-progress-title">{t('todo.panel.title')}</span>
        <span class="todo-progress-summary">{summary}</span>
        <svg class="todo-progress-chevron" width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path d="m4 6 4 4 4-4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
      </button>
      {expanded && (
        <ol class="todo-progress-list">
          {items.map((item, index) => (
            <li class={`todo-progress-item ${item.status}`} key={`${index}-${item.content}`}>
              <span class="todo-progress-status" role="img" aria-label={t(`todo.panel.status.${item.status}` as MsgKey)}>
                {item.status === 'completed' ? (
                  <svg width="18" height="18" viewBox="0 0 18 18" fill="none" aria-hidden="true">
                    <circle cx="9" cy="9" r="7" stroke="currentColor" stroke-width="1.5" />
                    <path d="m5.7 9.1 2.1 2.1 4.6-4.8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
                  </svg>
                ) : (
                  <span class="todo-progress-ring" aria-hidden="true" />
                )}
              </span>
              <span class="todo-progress-text">{item.content}</span>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

/** Render an assistant message's ordered parts in chronological order: each
 *  text run becomes Markdown; runs of consecutive tool calls share one
 *  `.tool-list` container. This is what preserves the text→tool→text→tool
 *  interleaving (matching the TUI) instead of grouping all tools at the head. */
function AssistantMessageView({
  msg,
  isLast,
  busy,
  isLastInTurn,
  turnText,
  artifacts,
  sessionId,
  searchRef,
  timeLabel,
  search,
  isActiveSearchMatch,
}: {
  msg: Message;
  isLast: boolean;
  busy: boolean;
  lastIdx: number;
  isLastInTurn: boolean;
  turnText: string;
  artifacts: TurnArtifact[];
  sessionId: string | null;
  searchRef?: (el: HTMLElement | null) => void;
  timeLabel?: string;
  search: string;
  isActiveSearchMatch: boolean;
}) {
  const t = useT();
  const text = messageText(msg);
  const isError =
    text.includes('[错误:') ||
    text.includes('[连接错误:') ||
    text.includes('[Error:') ||
    text.includes('[Connection error:');
  const streaming = isLast && busy;
  // 三点 loading：busy 且 assistant 气泡尚无任何可见输出。首个文本 / 推理 /
  // 工具事件到达即消失——文本/工具使 parts 非空，推理事件置 outputStarted。
  const waiting = streaming && msg.parts.length === 0 && !msg.outputStarted;
  // 终条且简短（无工具、单行）时，去掉多余 of "时间线末端"橙点，只留一个起始点。
  const terse =
    isLast && !streaming && !messageHasTools(msg) && !text.includes('\n');
  const dotClass = isError ? 'dot-error' : 'dot-brand';
  const cls =
    'timeline-message ' +
    dotClass +
    (streaming ? ' dot-blink' : '') +
    (isLast ? ' is-last' : '') +
    (terse ? ' is-terse' : '') +
    (isActiveSearchMatch ? ' is-active-search-match' : '');

  // Copy button: only shown on the last assistant message in a turn.
  // Copies the entire turn's text (all assistant messages in this turn joined).
  const [copied, setCopied] = useState(false);
  const [artifactError, setArtifactError] = useState('');

  function handleCopy() {
    navigator.clipboard.writeText(turnText).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }).catch(() => {});
  }

  const copyBtn = isLastInTurn && !isError && !streaming && turnText ? (
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
  ) : null;

  const showTimestamp = Boolean(timeLabel) && shouldShowAssistantTimestamp(isLastInTurn, streaming, text, isError);

  return (
    <div class={cls} ref={searchRef}>
      {/* Error turns are pure injected text — render flat. */}
      {isError ? (
        <div class="error-message-content">
          {highlightText(text, search)}
          {streaming && <span class="streaming-cursor" />}
        </div>
      ) : (
        <>
          {/* Segments in chronological order: text→tool→text→tool,
              matching the TUI. Consecutive tools share one tool-list. */}
          {renderAssistantParts(msg.parts, search)}
          {waiting ? (
            <span class="waiting-dots" role="status" aria-label={t('chat.waiting')}>
              <span class="waiting-dot" />
              <span class="waiting-dot" />
              <span class="waiting-dot" />
            </span>
          ) : (
            streaming && <span class="streaming-cursor" />
          )}
        </>
      )}
      {isLastInTurn && !streaming && artifacts.length > 0 && (
        <div class="turn-artifacts" aria-label={t('chat.artifacts')}>
          <span class="turn-artifacts-label">{t('chat.artifacts')}</span>
          <div class="turn-artifacts-list">
            {artifacts.slice(0, 6).map((artifact) => (
              <button
                key={artifact.path}
                type="button"
                class="turn-artifact-chip"
                title={artifact.path}
                onClick={() => {
                  setArtifactError('');
                  void openWorkspaceFile(artifact.path, sessionId ?? undefined).catch((error) => {
                    setArtifactError(t('chat.artifactOpenFailed', { msg: String(error) }));
                  });
                }}
              >
                {artifact.label}
              </button>
            ))}
            {artifacts.length > 6 && (
              <span class="turn-artifact-more">+{artifacts.length - 6}</span>
            )}
          </div>
          {artifactError && <div class="turn-artifact-error" role="status">{artifactError}</div>}
        </div>
      )}
      {(copyBtn || showTimestamp) && (
        <div class="msg-actions msg-actions-left">
          {copyBtn}
          {/* One timestamp per user turn: intermediate assistant/tool rounds share
              the turn's persisted timestamp but must not repeat it in the timeline. */}
          {showTimestamp && <div class="msg-time">{timeLabel}</div>}
        </div>
      )}
    </div>
  );
}

function renderAssistantParts(parts: MsgPart[], search: string): VNode[] {
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
      const visibleTools = tools.filter(shouldShowToolRow);
      if (visibleTools.length > 0) {
        out.push(
          <div class="tool-list" key={`tg-${groupKey}`}>
            {visibleTools.map((tool) => (
              <ToolRowView key={tool.id} tool={tool} />
            ))}
          </div>,
        );
      }
    } else if (p.kind === 'notice') {
      out.push(
        <div class="msg-notice" key={`nt-${i}`}>
          {highlightText(p.text, search)}
        </div>,
      );
      i++;
    } else if (p.kind === 'rate_limited') {
      out.push(
        <div class="rate-limited-notice" key={`rl-${i}`}>
          {highlightText(p.text, search)}
        </div>,
      );
      i++;
    } else {
      if (p.text) out.push(<Markdown key={`tx-${i}`} content={p.text} search={search} />);
      i++;
    }
  }
  return out;
}

function highlightText(text: string, search: string) {
  if (!search.trim()) return text;
  const searchLower = search.toLowerCase();
  const index = text.toLowerCase().indexOf(searchLower);
  if (index === -1) return text;
  
  const parts: (string | VNode)[] = [];
  let lastIndex = 0;
  let idx = index;
  let key = 0;
  while (idx !== -1) {
    if (idx > lastIndex) {
      parts.push(text.substring(lastIndex, idx));
    }
    parts.push(
      <mark key={key++} class="msg-search-highlight">
        {text.substring(idx, idx + search.length)}
      </mark>
    );
    lastIndex = idx + search.length;
    idx = text.toLowerCase().indexOf(searchLower, lastIndex);
  }
  if (lastIndex < text.length) {
    parts.push(text.substring(lastIndex));
  }
  return parts;
}

function UserMessageView({
  msg,
  searchRef,
  timeLabel,
  search,
  isActiveSearchMatch,
  steerPending,
}: {
  msg: Message;
  searchRef?: (el: HTMLElement | null) => void;
  timeLabel?: string;
  search: string;
  isActiveSearchMatch: boolean;
  steerPending?: boolean;
}) {
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

  const wrapperClass = 'user-message-wrapper'
    + (isActiveSearchMatch ? ' is-active-search-match' : '')
    + (steerPending ? ' steer-pending' : '');
  // A steer accepted mid-turn stays badged until the runtime folds it into the
  // turn (the fold ack drops it from pendingSteers) — so the user can see, on
  // their own message, exactly when it goes from "queued" to applied.
  const steerBadge = steerPending && (
    <div class="steer-pending-badge" role="status">{t('chat.steerBadge')}</div>
  );

  if (skillTitle && !expanded) {
    return (
      <div class={wrapperClass} ref={searchRef}>
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
        {steerBadge}
        <div class="msg-actions">
          {timeLabel && <div class="msg-time msg-time-user">{timeLabel}</div>}
          {copyBtn}
        </div>
      </div>
    );
  }

  return (
    <div class={wrapperClass} ref={searchRef}>
      <div class={'user-message-bubble' + (skillTitle ? ' is-markdown' : '')}>
        {images}
        {skillTitle && (
          <button class="skill-collapse" onClick={() => setExpanded(false)}>
            {t('chat.skillCollapse')}
          </button>
        )}
        {/* 技能/文档型内容本就是 markdown（注入的 SKILL.md），渲染它；
            普通用户消息保持逐字纯文本（不把用户输入当 markdown 解析）。 */}
        {skillTitle ? <Markdown content={text} search={search} /> : highlightText(text, search)}
      </div>
      <div class="msg-actions">
        {timeLabel && <div class="msg-time msg-time-user">{timeLabel}</div>}
        {copyBtn}
      </div>
      {steerBadge}
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
      ) : cls === 'warning' ? (
        <path d="M8 2.5 14 13H2L8 2.5ZM8 6v3.5M8 11.5h.01" />
      ) : (
        <path d="M8 2.5a5.5 5.5 0 1 1-5.18 3.65" />
      )}
    </svg>
  );
}

function shouldShowToolRow(tool: ToolRow): boolean {
  return !isTodoTool(tool.name) || tool.status === 'error' || tool.status === 'incomplete';
}

function ToolRowView({ tool }: { tool: ToolRow }) {
  const t = useT();
  const [expanded, setExpanded] = useState(false);

  // Successful TodoWrite calls are represented by the persistent composer
  // panel. Keep failures in the transcript so malformed plans remain visible.
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
  } else if (tool.status === 'incomplete') {
    annotation = { cls: 'warning', label: t('tool.incomplete') };
  }

  const hasDetail = !!(tool.args || tool.output);

  return (
    <div class="tool-body">
      <div class="tool-header" onClick={() => setExpanded((e) => !e)}>
        <ToolTypeIcon name={tool.name} />
        <span class="tool-name">{displayToolName(tool.name)}</span>
        <span class="tool-name-secondary">{abbreviateArgs(tool.detail ?? formatToolDetail(tool.name, tool.args))}</span>
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
      {tool.status === 'pending' && tool.progress && (
        <div class="tool-progress" title={tool.progress}>{tool.progress}</div>
      )}
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
