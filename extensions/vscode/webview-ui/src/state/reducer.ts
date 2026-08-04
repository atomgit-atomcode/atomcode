import type {
  ChatState,
  ChatAction,
  ChatMessage,
  ToolCallData,
  ContextFile,
  ArtifactData,
  MessageBlock,
  PermissionRequestData,
  StatusData,
  SearchState,
  AuthStatus,
  ProviderInfo,
  SessionTerminalState,
} from './types';
import { blocksFromLegacyMessage } from './blocks';
import { applyTodoCall, reduceTodosFromMessages } from './todo';
import { buildSearchMatches } from '../utils/search';

let _msgCounter = 0;
function nextId(): string {
  return `msg-${Date.now()}-${++_msgCounter}`;
}

function providerSetupRequired(
  providers: ProviderInfo[],
  currentProvider: string,
  auth?: AuthStatus,
): boolean {
  if (providers.length === 0) return true;
  const current = providers.find((provider) => provider.name === currentProvider)
    ?? providers.find((provider) => provider.is_default);
  const authUnavailable = !auth?.logged_in || auth.expired === true;
  // Older daemons did not expose requires_login. Preserve their previous
  // conservative behaviour until the daemon is upgraded.
  return current?.requires_login === undefined
    ? authUnavailable
    : current.requires_login && authUnavailable;
}

function lastAssistantIndex(messages: ChatMessage[]): number {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (messages[i].role === 'assistant') return i;
  }
  return -1;
}

function artifactLanguage(artifactType: string, language?: string, title?: string): string {
  const titleExt = title?.trim().match(/\.([a-z0-9]+)$/i)?.[1];
  const candidates = [language, artifactType, titleExt];
  for (const candidate of candidates) {
    const normalized = (candidate ?? '').trim().toLowerCase();
    if (!normalized) continue;
    if (normalized === 'diff' || normalized === 'patch') return 'diff';
    if (normalized === 'typescript') return 'ts';
    if (normalized === 'javascript') return 'js';
    if (normalized === 'markdown') return 'md';
    if (normalized === 'html' || normalized === 'htm') return 'html';
    if (normalized === 'svg') return 'svg';
    if (normalized === 'mermaid') return 'mermaid';
    if (normalized === 'code') continue;
    return normalized.replace(/^\./, '');
  }
  return 'text';
}

function mapHistoryArtifact(artifact: {
  id: string;
  artifact_type?: string;
  artifactType?: string;
  title?: string;
  language?: string;
  content: string;
}): ArtifactData {
  const artifactType = artifact.artifactType ?? artifact.artifact_type ?? 'code';
  return {
    id: artifact.id,
    artifactType,
    title: artifact.title,
    language: artifactLanguage(artifactType, artifact.language, artifact.title),
    content: artifact.content,
    status: 'complete',
  };
}

function normalizeRole(role: string): 'user' | 'assistant' | 'tool' | 'system' | 'unknown' {
  const normalized = String(role || '').toLowerCase();
  if (normalized === 'user' || normalized === 'assistant' || normalized === 'tool' || normalized === 'system') {
    return normalized;
  }
  return 'unknown';
}

const INTERNAL_USER_PREFIXES = [
  '<system-reminder>',
  'You made code edits but have not verified them.',
  'Output limit hit — your last response was cut off',
  'Output limit hit. If the task is already complete',
  '[PLAN MODE',
  '[Context was compressed',
  '[Additional context from user]:',
  '[SYNTAX CHECK:',
  '[DEV SERVER ERROR',
  '[Auto-read from error:',
  '[Images returned by the tool calls above',
];

function isInternalHistoryUserMessage(text: string, synthetic?: boolean): boolean {
  if (synthetic === true) return true;
  const trimmed = text.trimStart();
  return INTERNAL_USER_PREFIXES.some((prefix) => trimmed.startsWith(prefix));
}

function internalOriginOf(message: { internal_origin?: string; internalOrigin?: string }): string | undefined {
  return message.internal_origin ?? message.internalOrigin;
}

function textFromContent(content: unknown): string {
  if (typeof content === 'string') return content;
  if (!content || typeof content !== 'object') return '';

  const value = content as {
    Text?: unknown;
    AssistantWithToolCalls?: { text?: unknown };
    ToolResult?: { output?: unknown };
    ToolResultRef?: { summary?: unknown };
  };

  if (typeof value.Text === 'string') return value.Text;
  if (value.AssistantWithToolCalls) {
    return typeof value.AssistantWithToolCalls.text === 'string' ? value.AssistantWithToolCalls.text : '';
  }
  if (value.ToolResult) {
    return typeof value.ToolResult.output === 'string' ? value.ToolResult.output : '';
  }
  if (value.ToolResultRef) {
    return typeof value.ToolResultRef.summary === 'string' ? value.ToolResultRef.summary : '';
  }

  return '';
}

function currentBlocks(message: ChatMessage): MessageBlock[] {
  return message.blocks ? [...message.blocks] : blocksFromLegacyMessage(message);
}

function appendTextBlock(message: ChatMessage, content: string): ChatMessage {
  if (!content) return message;
  const blocks = currentBlocks(message);
  const last = blocks[blocks.length - 1];
  const nextBlocks = last?.type === 'text'
    ? [
        ...blocks.slice(0, -1),
        { ...last, content: last.content + content },
      ]
    : [
        ...blocks,
        { id: `${message.id}-text-${blocks.length}`, type: 'text' as const, content },
      ];
  return { ...message, text: message.text + content, blocks: nextBlocks };
}

function upsertToolBlock(message: ChatMessage, tool: ToolCallData): ChatMessage {
  const blocks = currentBlocks(message);
  const existing = blocks.findIndex((block) => block.type === 'tool' && block.tool.id === tool.id);
  const nextBlocks = existing >= 0
    ? blocks.map((block, index) => index === existing && block.type === 'tool' ? { ...block, tool } : block)
    : [...blocks, { id: `${message.id}-tool-${tool.id}`, type: 'tool' as const, tool }];
  return { ...message, blocks: nextBlocks };
}

function upsertArtifactMetadataBlock(message: ChatMessage, artifact: ArtifactData): ChatMessage {
  const blocks = currentBlocks(message);
  const existing = blocks.findIndex((block) => block.type === 'artifact' && block.artifact.id === artifact.id);
  const nextBlocks = existing >= 0
    ? blocks.map((block, index) =>
        index === existing && block.type === 'artifact'
          ? {
              ...block,
              artifact: {
                ...block.artifact,
                artifactType: artifact.artifactType,
                title: artifact.title ?? block.artifact.title,
                language: artifact.language,
                content: block.artifact.content || artifact.content,
                status: artifact.status,
              },
            }
          : block,
      )
    : [...blocks, { id: `${message.id}-artifact-${artifact.id}`, type: 'artifact' as const, artifact }];
  return { ...message, blocks: nextBlocks };
}

function mergeArtifactContent(existing: string, incoming: string): string {
  if (!existing) return incoming;
  if (!incoming) return existing;
  return existing + incoming;
}

function appendArtifactContentBlock(message: ChatMessage, id: string, content: string): ChatMessage {
  if (!content) return message;
  const blocks = currentBlocks(message);
  const existing = blocks.findIndex((block) => block.type === 'artifact' && block.artifact.id === id);
  const nextBlocks = existing >= 0
    ? blocks.map((block, index) =>
        index === existing && block.type === 'artifact'
          ? {
              ...block,
              artifact: {
                ...block.artifact,
                content: mergeArtifactContent(block.artifact.content, content),
                status: 'streaming' as const,
              },
            }
          : block,
      )
    : [
        ...blocks,
        {
          id: `${message.id}-artifact-${id}`,
          type: 'artifact' as const,
          artifact: {
            id,
            artifactType: 'code',
            language: 'text',
            content,
            status: 'streaming' as const,
          },
        },
      ];
  return { ...message, blocks: nextBlocks };
}

function completeArtifactBlock(message: ChatMessage, id: string): ChatMessage {
  const blocks = currentBlocks(message).map((block) =>
    block.type === 'artifact' && block.artifact.id === id
      ? { ...block, artifact: { ...block.artifact, status: 'complete' as const } }
      : block,
  );
  return { ...message, blocks };
}

function upsertPermissionBlock(message: ChatMessage, request: PermissionRequestData): ChatMessage {
  const blocks = currentBlocks(message);
  const existing = blocks.findIndex((block) => block.type === 'permission' && block.request.id === request.id);
  const nextBlocks = existing >= 0
    ? blocks.map((block, index) => index === existing && block.type === 'permission' ? { ...block, request } : block)
    : [...blocks, { id: `${message.id}-permission-${request.id}`, type: 'permission' as const, request }];
  return { ...message, blocks: nextBlocks };
}

function appendStatusBlock(message: ChatMessage, status: StatusData): ChatMessage {
  const blocks = currentBlocks(message);
  return {
    ...message,
    blocks: [
      ...blocks,
      { id: `${message.id}-status-${status.kind}-${blocks.length}`, type: 'status' as const, status },
    ],
  };
}

function upsertStatusBlock(message: ChatMessage, status: StatusData): ChatMessage {
  const blocks = currentBlocks(message);
  const existing = blocks.findIndex((block) => block.type === 'status' && block.status.kind === status.kind);
  const nextBlocks = existing >= 0
    ? blocks.map((block, index) => index === existing && block.type === 'status' ? { ...block, status } : block)
    : [
        ...blocks,
        { id: `${message.id}-status-${status.kind}`, type: 'status' as const, status },
      ];
  return { ...message, blocks: nextBlocks };
}

function settleOpenTools(
  message: ChatMessage,
  status: 'incomplete' | 'error',
  output?: string,
): ChatMessage {
  const openStatuses = new Set<ToolCallData['status']>(['queued', 'running', 'waiting_approval']);
  const tools = message.toolCalls?.map((tool) =>
    openStatuses.has(tool.status)
      ? {
          ...tool,
          status,
          output: output ?? tool.output,
          success: status === 'error' ? false : tool.success,
        }
      : tool,
  );
  if (!tools) return message;
  return tools.reduce<ChatMessage>(
    (updatedMessage, tool) => upsertToolBlock(updatedMessage, tool),
    { ...message, toolCalls: tools },
  );
}

function mergeTerminalIntoHistory(
  messages: ChatMessage[],
  terminal?: SessionTerminalState,
): ChatMessage[] {
  if (!terminal) return messages;
  const next = [...messages];
  const assistantIndex = lastAssistantIndex(next);
  const assistant = assistantIndex >= 0 ? next[assistantIndex] : undefined;

  if (terminal.type === 'done') {
    if (assistant) {
      let settled = settleOpenTools({ ...assistant, streaming: false }, 'incomplete');
      if (terminal.stopReason && terminal.stopReason !== 'stopped') {
        settled = upsertStatusBlock(settled, {
          kind: 'warning',
          message: terminal.message || `The turn ended before completion (${terminal.stopReason}).`,
        });
      }
      next[assistantIndex] = settled;
    } else if (terminal.stopReason && terminal.stopReason !== 'stopped') {
      next.push({
        id: nextId(),
        role: 'error',
        text: terminal.message || `The turn ended before completion (${terminal.stopReason}).`,
        timestamp: Date.now(),
      });
    }
    return next;
  }

  if (terminal.type === 'stopped') {
    if (assistant) {
      next[assistantIndex] = settleOpenTools({ ...assistant, streaming: false }, 'incomplete');
    }
    return next;
  }

  if (assistant) {
    next[assistantIndex] = settleOpenTools({ ...assistant, streaming: false }, 'error', terminal.message);
  }
  if (next.at(-1)?.role !== 'error' || next.at(-1)?.text !== terminal.message) {
    next.push({
      id: nextId(),
      role: 'error',
      text: terminal.message,
      timestamp: Date.now(),
    });
  }
  return next;
}

function updatePermissionBlock(
  message: ChatMessage,
  id: string,
  update: (request: PermissionRequestData) => PermissionRequestData,
): ChatMessage {
  const blocks = currentBlocks(message);
  let updatedRequest: PermissionRequestData | undefined;
  const nextBlocks = blocks.map((block) => {
    if (block.type !== 'permission' || block.request.id !== id) return block;
    updatedRequest = update(block.request);
    return { ...block, request: updatedRequest };
  });
  if (!updatedRequest) return message;
  return {
    ...message,
    blocks: nextBlocks,
    permissionRequest: message.permissionRequest?.id === id
      ? updatedRequest
      : message.permissionRequest,
  };
}

// Matches the prefix emitted in provider.ts _handleSend when context files are attached.
const ATTACHED_FILES_PREFIX = /^The user has attached the following file\(s\)(?:\/selection\(s\))? for context\./;

function parseAttachedMessage(rawText: string): { displayText: string; contextFiles: ContextFile[] } {
  if (!ATTACHED_FILES_PREFIX.test(rawText)) {
    return { displayText: rawText, contextFiles: [] };
  }

  const questionMarker = '\n\nUser question: ';
  const questionIdx = rawText.lastIndexOf(questionMarker);
  const userQuestion = questionIdx >= 0 ? rawText.slice(questionIdx + questionMarker.length).trim() : rawText;

  // Extract file names from ```<ext> fenced blocks.
  const contextFiles: ContextFile[] = [];
  const filePattern = /^File: (.+?)(?: \(lines (\d+)-(\d+)\))?$/gm;
  let match: RegExpExecArray | null;
  while ((match = filePattern.exec(rawText)) !== null) {
    const fileName = match[1];
    const startLine = match[2] ? Number(match[2]) : undefined;
    const endLine = match[3] ? Number(match[3]) : undefined;
    if (!contextFiles.some((f) => f.fileName === fileName)) {
      contextFiles.push({
        path: fileName,
        fileName,
        type: startLine && endLine ? 'selection' : 'file',
        startLine,
        endLine,
      });
    }
  }

  return { displayText: userQuestion, contextFiles };
}

function stripVisionPreprocessText(rawText: string): { displayText: string; hadVisionMarker: boolean } {
  const markerIndex = rawText.indexOf('[图片内容（由');
  const failureIndex = rawText.indexOf('[图片识别失败]');
  const indexes = [markerIndex, failureIndex].filter((index) => index >= 0);
  if (indexes.length === 0) {
    return { displayText: rawText, hadVisionMarker: false };
  }

  return {
    displayText: rawText.slice(0, Math.min(...indexes)).trimEnd(),
    hadVisionMarker: true,
  };
}

const EMPTY_SEARCH: SearchState = { matches: [], currentMatchIndex: -1 };

/** Recompute search matches for the given query. Keeps the current index
 *  clamped to the new range so navigation stays valid after edits. */
function recomputeSearch(
  messages: ChatMessage[],
  query: string,
  prev?: SearchState,
): SearchState {
  const trimmed = query.trim();
  if (!trimmed) return EMPTY_SEARCH;
  const matches = buildSearchMatches(messages, query);
  if (matches.length === 0) return { matches, currentMatchIndex: -1 };
  let nextIndex = prev && prev.currentMatchIndex >= 0 ? prev.currentMatchIndex : 0;
  if (nextIndex >= matches.length) nextIndex = 0;
  return { matches, currentMatchIndex: nextIndex };
}

export const initialState: ChatState = {
  messages: [],
  queuedMessages: [],
  activeTodos: [],
  isGenerating: false,
  recoveryLocked: false,
  isSessionList: document.body.dataset.viewMode === 'sidebar',
  viewMode: document.body.dataset.viewMode === 'sidebar' ? 'sidebar' : 'tab',
  currentModel: 'default',
  currentProvider: '',
  models: [],
  providers: [],
  auth: undefined,
  setupRequired: false,
  setupStatus: undefined,
  setupError: undefined,
  loginUrl: undefined,
  sessions: [],
  activeSessionId: undefined,
  activeProjectHash: undefined,
  contextFiles: [],
  tokenCount: undefined,
  historyOpen: false,
  settingsOpen: false,
  searchQuery: '',
  searchOpen: false,
  search: EMPTY_SEARCH,
  locale: document.body.dataset.locale,
  approvalMode: 'build',
  approvalModePending: false,
  persistenceWarning: undefined,
};

export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  const next = chatReducerInner(state, action);
  // Keep search matches in sync when messages change and a query is active.
  if (next.messages !== state.messages && next.searchQuery.trim()) {
    const recomputed = recomputeSearch(next.messages, next.searchQuery, next.search);
    // Skip if matches haven't meaningfully changed (avoids unnecessary re-renders).
    const sameMatches =
      recomputed.matches.length === next.search.matches.length &&
      recomputed.matches.every((m, idx) =>
        m.messageId === next.search.matches[idx]?.messageId &&
        m.ranges.length === next.search.matches[idx]?.ranges.length);
    if (!sameMatches || recomputed.currentMatchIndex !== next.search.currentMatchIndex) {
      return { ...next, search: recomputed };
    }
  }
  return next;
}

function chatReducerInner(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    // ─── User sends a message ────────────────────────
    case 'ADD_USER_MESSAGE': {
      const msg: ChatMessage = {
        id: nextId(),
        role: 'user',
        text: action.text,
        contextFiles: action.contextFiles,
        images: action.images,
        timestamp: Date.now(),
      };
      return { ...state, messages: [...state.messages, msg] };
    }

    case 'ADD_QUEUED_MESSAGE': {
      const msg: ChatMessage = {
        id: action.id,
        role: 'user',
        text: action.text,
        queued: true,
        contextFiles: action.contextFiles,
        images: action.images,
        timestamp: Date.now(),
      };
      return { ...state, queuedMessages: [...state.queuedMessages, msg] };
    }

    case 'SEND_QUEUED_MESSAGE': {
      const queued = state.queuedMessages.find((msg) => msg.id === action.id);
      if (!queued) return state;
      return {
        ...state,
        messages: [...state.messages, { ...queued, queued: false }],
        queuedMessages: state.queuedMessages.filter((msg) => msg.id !== action.id),
      };
    }

    case 'CLEAR_QUEUED_MESSAGES':
      return {
        ...state,
        queuedMessages: [],
      };

    case 'ADD_ASSISTANT_MESSAGE': {
      const id = nextId();
      const msg: ChatMessage = {
        id,
        role: 'assistant',
        text: action.text,
        blocks: action.text ? [{ id: `${id}-text-0`, type: 'text', content: action.text }] : [],
        toolCalls: [],
        streaming: false,
        timestamp: Date.now(),
      };
      return { ...state, messages: [...state.messages, msg] };
    }

    // ─── Generation lifecycle ────────────────────────
    case 'START_GENERATION': {
      const assistant: ChatMessage = {
        id: nextId(),
        role: 'assistant',
        text: '',
        blocks: [],
        toolCalls: [],
        streaming: true,
        timestamp: Date.now(),
      };
      return {
        ...state,
        isGenerating: true,
        recoveryLocked: false,
        persistenceWarning: undefined,
        messages: [...state.messages, assistant],
      };
    }

    // Resume a session that has an active background stream. Same as
    // START_GENERATION: create a fresh streaming assistant message that
    // subsequent text/toolStart events will append to.
    case 'RESUME_STREAMING': {
      if (state.messages.some((message) => message.role === 'assistant' && message.streaming)) {
        return state.isGenerating && !state.recoveryLocked
          ? state
          : { ...state, isGenerating: true, recoveryLocked: false };
      }
      const assistant: ChatMessage = {
        id: nextId(),
        role: 'assistant',
        text: '',
        blocks: [],
        toolCalls: [],
        streaming: true,
        timestamp: Date.now(),
      };
      return {
        ...state,
        isGenerating: true,
        recoveryLocked: false,
        messages: [...state.messages, assistant],
      };
    }

    case 'APPEND_TEXT': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = appendTextBlock(assistant, action.content);
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_BATCH_START': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        const existingIds = new Set((assistant.toolCalls ?? []).map((tool) => tool.id));
        const tools: ToolCallData[] = action.calls
          .filter((call) => !existingIds.has(call.id))
          .map((c) => ({
            id: c.id,
            name: c.name,
            args: c.args,
            status: 'queued' as const,
          }));
        msgs[assistantIndex] = {
          ...assistant,
          toolCalls: [...(assistant.toolCalls ?? []), ...tools],
        };
        msgs[assistantIndex] = tools.reduce((message, tool) => upsertToolBlock(message, tool), msgs[assistantIndex]);
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_START': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      let shouldApplyTodo = false;
      if (assistant) {
        const existingIndex = assistant.toolCalls?.findIndex((t) => t.id === action.id);
        if (existingIndex !== undefined && existingIndex >= 0) {
          const existingStatus = assistant.toolCalls![existingIndex].status;
          shouldApplyTodo = existingStatus === 'queued' || existingStatus === 'waiting_approval';
          if (shouldApplyTodo) {
            // Tool was already announced via TOOL_BATCH_START — transition to running.
            // Replayed starts must not reopen terminal calls.
            const updated = assistant.toolCalls!.map((t, i) =>
              i === existingIndex ? { ...t, args: action.args, status: 'running' as const } : t,
            );
            const updatedTool = updated[existingIndex];
            msgs[assistantIndex] = upsertToolBlock({ ...assistant, toolCalls: updated }, updatedTool);
          }
        } else {
          shouldApplyTodo = true;
          // Legacy path: tool wasn't in a batch, add it directly as running
          const tool: ToolCallData = {
            id: action.id,
            name: action.name,
            args: action.args,
            status: 'running',
          };
          msgs[assistantIndex] = {
            ...assistant,
            toolCalls: [...(assistant.toolCalls ?? []), tool],
          };
          msgs[assistantIndex] = upsertToolBlock(msgs[assistantIndex], tool);
        }
      }
      return {
        ...state,
        messages: msgs,
        activeTodos: shouldApplyTodo
          ? applyTodoCall(state.activeTodos, action.name, action.args)
          : state.activeTodos,
      };
    }

    case 'TOOL_PROGRESS': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant?.toolCalls) {
        const tools = assistant.toolCalls.map((tool) =>
          tool.id === action.id ? { ...tool, progress: action.progress } : tool,
        );
        const updatedTool = tools.find((tool) => tool.id === action.id);
        msgs[assistantIndex] = updatedTool
          ? upsertToolBlock({ ...assistant, toolCalls: tools }, updatedTool)
          : { ...assistant, toolCalls: tools };
      }
      return { ...state, messages: msgs };
    }

    case 'TOOL_RESULT': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant?.toolCalls) {
        const status = action.success
          ? 'done' as const
          : action.output.startsWith('Code review incomplete')
            ? 'incomplete' as const
            : 'error' as const;
        const tools = assistant.toolCalls.map((t) =>
          t.id === action.id
            ? { ...t, output: action.output, success: action.success, durationMs: action.durationMs, progress: undefined, status }
            : t,
        );
        const updatedTool = tools.find((tool) => tool.id === action.id);
        msgs[assistantIndex] = updatedTool
          ? upsertToolBlock({ ...assistant, toolCalls: tools }, updatedTool)
          : { ...assistant, toolCalls: tools };
      }
      return { ...state, messages: msgs };
    }

    case 'STREAM_WARNING': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = appendStatusBlock(assistant, {
          kind: 'warning',
          message: action.message,
        });
      }
      return { ...state, messages: msgs };
    }

    case 'SET_PERSISTENCE_WARNING':
      return { ...state, persistenceWarning: action.message };

    case 'STREAM_RATE_LIMITED': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = upsertStatusBlock(assistant, {
          kind: 'rate_limited',
          message: action.message,
          retryAfterSeconds: action.retryAfterSeconds,
          attempt: action.attempt,
          maxAttempts: action.maxAttempts,
        });
      }
      return { ...state, messages: msgs };
    }

    case 'STREAM_IDLE_NOTICE': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = upsertStatusBlock(assistant, {
          kind: 'idle',
          message: action.message,
        });
      }
      return { ...state, messages: msgs };
    }

    case 'ARTIFACT_START': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        const artifact: ArtifactData = {
          id: action.id,
          artifactType: action.artifactType,
          title: action.title,
          language: artifactLanguage(action.artifactType, action.language, action.title),
          content: '',
          status: 'streaming',
        };
        const existing = assistant.artifacts ?? [];
        const nextArtifacts = existing.some((a) => a.id === action.id)
          ? existing.map((a) =>
              a.id === action.id
                ? {
                    ...a,
                    artifactType: action.artifactType,
                    title: action.title ?? a.title,
                    language: artifact.language,
                    status: 'streaming' as const,
                  }
                : a,
            )
          : [...existing, artifact];
        msgs[assistantIndex] = upsertArtifactMetadataBlock({ ...assistant, artifacts: nextArtifacts }, artifact);
      }
      return { ...state, messages: msgs };
    }

    case 'ARTIFACT_CONTENT': {
      if (!action.content) return state;
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        const artifacts = assistant.artifacts ?? [];
        const nextArtifacts = artifacts.some((a) => a.id === action.id)
          ? artifacts.map((a) =>
              a.id === action.id ? { ...a, content: mergeArtifactContent(a.content, action.content), status: 'streaming' as const } : a,
            )
          : [{
              id: action.id,
              artifactType: 'code',
              language: 'text',
              content: action.content,
              status: 'streaming' as const,
            }];
        msgs[assistantIndex] = appendArtifactContentBlock({ ...assistant, artifacts: nextArtifacts }, action.id, action.content);
      }
      return { ...state, messages: msgs };
    }

    case 'ARTIFACT_END': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant?.artifacts) {
        msgs[assistantIndex] = {
          ...assistant,
          artifacts: assistant.artifacts.map((artifact) =>
            artifact.id === action.id ? { ...artifact, status: 'complete' as const } : artifact,
          ),
        };
        msgs[assistantIndex] = completeArtifactBlock(msgs[assistantIndex], action.id);
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
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = settleOpenTools({ ...assistant, streaming: false }, 'incomplete');
      }
      // action.tokens is a number (total), not a tokenCount object
      const tokenCount = typeof action.tokens === 'number'
        ? { prompt: 0, completion: 0, total: action.tokens }
        : state.tokenCount;
      return {
        ...state,
        isGenerating: false,
        recoveryLocked: false,
        messages: msgs,
        tokenCount,
      };
    }

    case 'GENERATION_STOPPED': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = settleOpenTools({ ...assistant, streaming: false }, 'incomplete');
      }
      return { ...state, isGenerating: false, recoveryLocked: false, messages: msgs, queuedMessages: [] };
    }

    case 'GENERATION_ERROR': {
      const msgs = [...state.messages];
      const assistantIndex = lastAssistantIndex(msgs);
      const assistant = assistantIndex >= 0 ? msgs[assistantIndex] : undefined;
      if (assistant) {
        msgs[assistantIndex] = settleOpenTools({ ...assistant, streaming: false }, 'error', action.message);
      }
      const errMsg: ChatMessage = {
        id: nextId(),
        role: 'error',
        text: action.message,
        timestamp: Date.now(),
      };
      return { ...state, isGenerating: false, recoveryLocked: false, messages: [...msgs, errMsg], queuedMessages: [] };
    }

    case 'RECOVERY_REQUIRED':
      return { ...state, recoveryLocked: true, queuedMessages: [] };

    case 'RECOVERY_CLEARED':
      return { ...state, recoveryLocked: false };

    // ─── Session management ─────────────────────────
    case 'CLEAR_CHAT':
      return { ...state, messages: [], queuedMessages: [], activeTodos: [], tokenCount: undefined, contextFiles: [], isGenerating: false, recoveryLocked: false, persistenceWarning: undefined };

    case 'SET_MODELS': {
      const hasCurrent = action.models.some((m) => m.provider === state.currentProvider);
      const current = hasCurrent
        ? action.models.find((m) => m.provider === state.currentProvider)
        : action.models.find((m) => m.is_default);
      return {
        ...state,
        models: action.models,
        currentProvider: current?.provider ?? state.currentProvider,
        currentModel: current?.model ?? state.currentModel,
      };
    }

    case 'SET_PROVIDERS': {
      const current = action.providers.find((p) => p.name === action.defaultProvider)
        ?? action.providers.find((p) => p.is_default);
      return {
        ...state,
        providers: action.providers,
        currentProvider: current?.name ?? state.currentProvider,
        currentModel: current?.model ?? state.currentModel,
        setupRequired: providerSetupRequired(
          action.providers,
          current?.name ?? state.currentProvider,
          state.auth,
        ),
      };
    }

    case 'SET_AUTH':
      return {
        ...state,
        auth: action.auth,
        setupRequired: providerSetupRequired(state.providers, state.currentProvider, action.auth),
      };

    case 'SET_SETUP_STATE': {
      const current = action.providers.find((p) => p.name === action.defaultProvider)
        ?? action.providers.find((p) => p.is_default);
      return {
        ...state,
        auth: action.auth ?? state.auth,
        providers: action.providers,
        currentProvider: current?.name ?? action.defaultProvider ?? state.currentProvider,
        currentModel: action.currentModel ?? current?.model ?? state.currentModel,
        setupRequired: action.setupRequired,
        setupError: undefined,
        setupStatus: action.setupRequired ? state.setupStatus : undefined,
      };
    }

    case 'SET_SETUP_STATUS':
      return {
        ...state,
        setupStatus: action.status,
        setupError: action.error,
        loginUrl: action.loginUrl ?? state.loginUrl,
      };

    case 'SET_CURRENT_MODEL':
      return { ...state, currentModel: action.model };

    case 'SET_CURRENT_PROVIDER': {
      const provider = state.providers.find((p) => p.name === action.provider);
      return {
        ...state,
        currentProvider: action.provider,
        currentModel: action.model ?? provider?.model ?? state.currentModel,
        setupRequired: providerSetupRequired(state.providers, action.provider, state.auth),
      };
    }

    case 'SET_REASONING_EFFORT':
      return {
        ...state,
        models: state.models.map((m) =>
          m.provider === action.provider ? { ...m, reasoning_effort: action.effort } : m,
        ),
      };

    case 'SET_APPROVAL_MODE':
      return { ...state, approvalMode: action.mode, approvalModePending: action.pending ?? false };

    case 'SET_SESSIONS':
      return { ...state, sessions: action.sessions };

    case 'SET_ACTIVE_SESSION':
      return {
        ...state,
        activeSessionId: action.sessionId,
        activeTodos: action.sessionId === state.activeSessionId ? state.activeTodos : [],
        activeProjectHash: action.projectHash
          ?? (action.sessionId === state.activeSessionId ? state.activeProjectHash : undefined),
      };

    // ─── Context files ──────────────────────────────
    case 'ADD_CONTEXT_FILE': {
      // For selections, use path+startLine as unique key; for files, use path only
      const isDup = action.file.type === 'selection'
        ? state.contextFiles.some((f) => f.path === action.file.path && f.startLine === action.file.startLine)
        : state.contextFiles.some((f) => f.path === action.file.path && f.type === 'file');
      if (isDup) return state;
      return { ...state, contextFiles: [...state.contextFiles, action.file] };
    }

    case 'REMOVE_CONTEXT_FILE':
      return {
        ...state,
        contextFiles: state.contextFiles.filter((f) =>
          action.startLine
            ? !(f.path === action.path && f.startLine === action.startLine)
            : f.path !== action.path
        ),
      };

    case 'CLEAR_CONTEXT':
      return { ...state, contextFiles: [] };

    case 'TOGGLE_HISTORY':
      return { ...state, historyOpen: !state.historyOpen };

    case 'TOGGLE_SETTINGS':
      return { ...state, settingsOpen: !state.settingsOpen };

    case 'LOAD_SESSION_MESSAGES': {
      // Convert daemon message format to our ChatMessage format
      const messages: ChatMessage[] = [];

      for (const m of action.messages) {
        const role = normalizeRole(m.role);

        if (role === 'tool') {
          // Find the last assistant message with tool calls — we need to update
          // it immutably (no direct mutation of objects already in the array).
          const lastAssistantIdx = messages.findLastIndex(
            (msg) => msg.role === 'assistant' && (msg.toolCalls?.length ?? 0) > 0,
          );
          if (lastAssistantIdx < 0) continue;

          const lastAssistant = messages[lastAssistantIdx];
          const callId = m.tool_result?.call_id;
          const output = textFromContent(m.content);

          if (lastAssistant.toolCalls && output) {
            const targetIndex = callId
              ? lastAssistant.toolCalls.findIndex((tool) => tool.id === callId)
              : lastAssistant.toolCalls.findIndex((tool) => tool.output === undefined);

            if (targetIndex >= 0) {
              // Immutable update: create new toolCalls array and new message object
              const newToolCalls = lastAssistant.toolCalls.map((tool, i) =>
                i === targetIndex
                  ? {
                      ...tool,
                      output,
                      success: m.tool_result?.success ?? true,
                      status: (m.tool_result?.success === false ? 'error' : 'done') as 'done' | 'error',
                    }
                  : tool,
              );
              const updatedTool = newToolCalls[targetIndex];
              messages[lastAssistantIdx] = upsertToolBlock({ ...lastAssistant, toolCalls: newToolCalls }, updatedTool);
            }
          }
          continue;
        }

        if (role !== 'user' && role !== 'assistant') {
          continue;
        }

        const toolCalls: ToolCallData[] = (m.tool_calls ?? []).map((tool, index) => ({
          id: tool.id || `history-tool-${index}`,
          name: tool.name || 'tool',
          args: tool.arguments || '',
          success: true,
          status: 'done',
        }));

        const rawText = textFromContent(m.content);
        if (role === 'user' && isInternalHistoryUserMessage(rawText, m.synthetic)) {
          continue;
        }
        const internalOrigin = internalOriginOf(m);
        if (role === 'assistant' && internalOrigin === 'verify_cadence' && toolCalls.length === 0) {
          continue;
        }
        const { displayText: userVisibleText, hadVisionMarker } = role === 'user'
          ? stripVisionPreprocessText(rawText)
          : { displayText: rawText, hadVisionMarker: false };

        // User messages may contain inline file content from the send path.
        // Parse it out into contextFiles so the UI shows attachment pills
        // instead of dumping the file body into the message bubble.
        const { displayText, contextFiles } = role === 'user'
          ? parseAttachedMessage(userVisibleText)
          : { displayText: userVisibleText, contextFiles: [] as ContextFile[] };
        const images = role === 'user' && m.images && m.images.length > 0
          ? m.images
          : role === 'user' && hadVisionMarker
            ? [{ media_type: 'image/png', data: '', missing: true }]
            : undefined;

        const message: ChatMessage = {
          id: nextId(),
          role,
          text: displayText,
          toolCalls,
          artifacts: role === 'assistant' && m.artifacts && m.artifacts.length > 0
            ? m.artifacts.map(mapHistoryArtifact)
            : undefined,
          contextFiles: contextFiles.length > 0 ? contextFiles : undefined,
          images,
          streaming: false,
          timestamp: Date.now(),
        };
        messages.push({ ...message, blocks: role === 'assistant' ? blocksFromLegacyMessage(message) : undefined });
      }
      const mergedMessages = mergeTerminalIntoHistory(messages, action.terminal);
      return {
        ...state,
        messages: mergedMessages,
        activeTodos: reduceTodosFromMessages(mergedMessages),
        isGenerating: false,
        persistenceWarning: undefined,
      };
    }

    case 'SET_SEARCH_QUERY': {
      const search = recomputeSearch(state.messages, action.query, state.search);
      return { ...state, searchQuery: action.query, search };
    }

    case 'TOGGLE_SEARCH': {
      const closing = state.searchOpen;
      return {
        ...state,
        searchOpen: !state.searchOpen,
        searchQuery: closing ? '' : state.searchQuery,
        search: closing ? EMPTY_SEARCH : recomputeSearch(state.messages, state.searchQuery),
      };
    }

    case 'SEARCH_NEXT': {
      const { matches, currentMatchIndex } = state.search;
      if (matches.length === 0) return state;
      const next = (currentMatchIndex + 1) % matches.length;
      return { ...state, search: { matches, currentMatchIndex: next } };
    }

    case 'SEARCH_PREV': {
      const { matches, currentMatchIndex } = state.search;
      if (matches.length === 0) return state;
      const prev = currentMatchIndex <= 0 ? matches.length - 1 : currentMatchIndex - 1;
      return { ...state, search: { matches, currentMatchIndex: prev } };
    }

    case 'PERMISSION_REQUEST': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        const toolCalls = last.toolCalls?.map((tool) =>
          tool.id === action.id ? { ...tool, status: 'waiting_approval' as const } : tool,
        );
        const request: PermissionRequestData = {
          id: action.id,
          sessionId: action.sessionId,
          toolName: action.toolName,
          reason: action.reason,
          args: action.args,
          isDestructive: action.isDestructive,
          status: 'pending',
        };
        let nextMessage = upsertPermissionBlock({
          ...last,
          toolCalls,
          permissionRequest: request,
        }, request);
        for (const tool of toolCalls ?? []) {
          nextMessage = upsertToolBlock(nextMessage, tool);
        }
        msgs[msgs.length - 1] = nextMessage;
      }
      return { ...state, messages: msgs };
    }

    case 'PERMISSION_RESPOND': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = updatePermissionBlock(last, action.id, (request) => ({
          ...request,
          status: 'submitting',
          decision: action.decision,
          error: undefined,
        }));
      }
      return { ...state, messages: msgs };
    }

    case 'PERMISSION_RESPONSE_RESULT': {
      const msgs = [...state.messages];
      const last = msgs[msgs.length - 1];
      if (last?.role === 'assistant') {
        msgs[msgs.length - 1] = updatePermissionBlock(last, action.id, (request) => action.success
          ? {
              ...request,
              status: request.decision === 'deny' ? 'denied' : 'allowed',
              error: undefined,
            }
          : {
              ...request,
              status: 'pending',
              decision: undefined,
              error: action.message || 'Permission response failed',
            });
      }
      return { ...state, messages: msgs };
    }

    // ─── Init ───────────────────────────────────────
    case 'INIT': {
      const activeSessionId = action.activeSessionId ?? state.activeSessionId;
      return {
        ...state,
        isGenerating: action.generating,
        recoveryLocked: action.recoveryLocked ?? false,
        currentModel: action.currentModel ?? state.currentModel,
        viewMode: action.viewMode ?? state.viewMode,
        activeSessionId,
        activeTodos: activeSessionId === state.activeSessionId ? state.activeTodos : [],
        activeProjectHash: action.projectHash ?? state.activeProjectHash,
        isSessionList: action.isSessionList ?? state.isSessionList,
        locale: action.locale ?? state.locale,
        approvalMode: action.approvalMode ?? state.approvalMode,
        approvalModePending: action.approvalModePending ?? state.approvalModePending,
      };
    }

    default:
      return state;
  }
}
