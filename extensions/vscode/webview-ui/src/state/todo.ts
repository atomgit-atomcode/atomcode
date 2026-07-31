import type { ChatMessage, TodoItemData, TodoStatus, ToolCallData } from './types';

type TodoOperation =
  | { kind: 'plan'; items: TodoItemData[] }
  | { kind: 'add'; content: string }
  | { kind: 'update'; id: number; status: TodoStatus }
  | { kind: 'list' };

const TODO_STATUSES = new Set<TodoStatus>(['pending', 'in_progress', 'completed']);

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
      || typeof value.status !== 'string'
      || !TODO_STATUSES.has(value.status as TodoStatus)) {
      return undefined;
    }
    return {
      kind: 'update',
      id: value.id as number,
      status: value.status as TodoStatus,
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
