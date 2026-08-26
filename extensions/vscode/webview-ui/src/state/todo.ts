import type { ChatMessage, TodoItemData, TodoStatus, ToolCallData } from './types';

type TodoOperation =
  | { kind: 'plan'; items: TodoItemData[] }
  | { kind: 'add'; content: string }
  | { kind: 'update'; id: number; status: TodoStatus }
  | { kind: 'list' };

const TODO_STATUSES = new Set<TodoStatus>(['pending', 'in_progress', 'completed']);

// Lenient status parse for incremental `update` calls, mirroring the Rust
// backend's `TodoStatus::parse_lenient` (issue #1456). Long-context turns
// routinely emit near-miss variants (`done`, `in progress`, `InProgress`,
// `已完成` …); rejecting them here would drop the update client-side and leave
// the panel stale while the backend already advanced the task. Full-list plans
// keep the strict `TODO_STATUSES` gate — the two sides must agree, and the Rust
// `parse_todos` plan path is strict too.
function normalizeTodoStatus(raw: string): TodoStatus | undefined {
  switch (raw.trim().toLowerCase()) {
    case 'pending': case 'todo': case 'open': case 'waiting':
    case 'not started': case 'not_started': case '待办': case '未开始':
      return 'pending';
    case 'in_progress': case 'in-progress': case 'in progress': case 'inprogress':
    case 'doing': case 'started': case 'active': case 'working': case 'running':
    case '进行中': case '开发中':
      return 'in_progress';
    case 'completed': case 'complete': case 'done': case 'finished':
    case 'closed': case 'resolved': case '已完成': case '完成':
      return 'completed';
    default:
      return undefined;
  }
}

export function isTodoToolName(name: string): boolean {
  return name === 'todowrite' || name === 'todo';
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function parseTodoOperation(name: string, args: string): TodoOperation | undefined {
  if (!isTodoToolName(name)) return undefined;

  let value: unknown;
  try {
    value = JSON.parse(args);
  } catch {
    return undefined;
  }
  if (!isRecord(value)) return undefined;

  if ('todos' in value) {
    if (!Array.isArray(value.todos)) return undefined;
    const items: TodoItemData[] = [];
    let inProgress = 0;
    for (const rawItem of value.todos) {
      if (!isRecord(rawItem)
        || typeof rawItem.content !== 'string'
        || rawItem.content.trim().length === 0
        || typeof rawItem.status !== 'string'
        || !TODO_STATUSES.has(rawItem.status as TodoStatus)) {
        return undefined;
      }
      const status = rawItem.status as TodoStatus;
      if (status === 'in_progress') inProgress += 1;
      items.push({ content: rawItem.content, status });
    }
    return inProgress <= 1 ? { kind: 'plan', items } : undefined;
  }

  if (value.action === 'add') {
    if (typeof value.content !== 'string' || value.content.trim().length === 0) return undefined;
    return { kind: 'add', content: value.content.trim() };
  }

  if (value.action === 'update') {
    if (!Number.isSafeInteger(value.id)
      || (value.id as number) < 1
      || typeof value.status !== 'string') {
      return undefined;
    }
    const status = normalizeTodoStatus(value.status);
    if (!status) return undefined;
    return {
      kind: 'update',
      id: value.id as number,
      status,
    };
  }

  // Older sessions may contain the retired stateful `todo` tool's list action.
  if (name === 'todo' && value.action === 'list') return { kind: 'list' };
  return undefined;
}

export function applyTodoCall(
  items: TodoItemData[],
  name: string,
  args: string,
): TodoItemData[] {
  const operation = parseTodoOperation(name, args);
  if (!operation || operation.kind === 'list') return items;
  if (operation.kind === 'plan') return operation.items;
  if (operation.kind === 'add') {
    return [...items, { content: operation.content, status: 'pending' }];
  }

  if (operation.id > items.length) return items;
  const targetIndex = operation.id - 1;
  return items.map((item, index) => {
    if (index === targetIndex) return { ...item, status: operation.status };
    if (operation.status === 'in_progress' && item.status === 'in_progress') {
      return { ...item, status: 'pending' };
    }
    return item;
  });
}

export function reduceTodosFromMessages(messages: ChatMessage[]): TodoItemData[] {
  let items: TodoItemData[] = [];
  for (const message of messages) {
    const calls = message.toolCalls && message.toolCalls.length > 0
      ? message.toolCalls
      : (message.blocks ?? [])
          .filter((block) => block.type === 'tool')
          .map((block) => block.tool);
    for (const call of calls) {
      items = applyTodoCall(items, call.name, call.args);
    }
  }
  return items;
}

export function shouldRenderToolCall(tool: ToolCallData): boolean {
  if (!isTodoToolName(tool.name)) return true;
  if (tool.status === 'error' || tool.status === 'incomplete') return true;
  return parseTodoOperation(tool.name, tool.args) === undefined;
}
