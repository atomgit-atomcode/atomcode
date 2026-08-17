export type TodoStatus = 'pending' | 'in_progress' | 'completed';

export interface TodoItem {
  content: string;
  status: TodoStatus;
}

export interface TodoProjectionCall {
  id: string;
  name: string;
  args: string;
  success?: boolean;
}

export interface TodoProjection {
  committed: TodoItem[];
  preview: TodoItem[];
  hasUnresolved: boolean;
}

function parseArgs(argsJson: string): Record<string, unknown> | null {
  try {
    const value = JSON.parse(argsJson);
    return value !== null && typeof value === 'object'
      ? value as Record<string, unknown>
      : null;
  } catch {
    return null;
  }
}

function parseStatus(value: unknown): TodoStatus | null {
  return value === 'pending' || value === 'in_progress' || value === 'completed'
    ? value
    : null;
}

function parseFullList(value: unknown): TodoItem[] | null {
  if (typeof value === 'string') {
    try { value = JSON.parse(value); } catch { return null; }
  }
  if (!Array.isArray(value)) return null;

  const items: TodoItem[] = [];
  let inProgress = 0;
  for (const raw of value) {
    if (raw === null || typeof raw !== 'object') return null;
    const item = raw as Record<string, unknown>;
    const content = typeof item.content === 'string' ? item.content.trim() : '';
    const status = parseStatus(item.status);
    if (!content || status === null) return null;
    if (status === 'in_progress') inProgress += 1;
    items.push({ content, status });
  }
  return inProgress <= 1 ? items : null;
}

export function isTodoTool(name: string): boolean {
  return name === 'todo' || name === 'todowrite';
}

/** Fold one successful TodoWrite call into transcript-derived UI state. */
export function applyTodoCall(
  current: readonly TodoItem[],
  name: string,
  argsJson: string,
): TodoItem[] {
  if (!isTodoTool(name)) return [...current];
  const args = parseArgs(argsJson);
  if (!args) return [...current];

  if (Object.prototype.hasOwnProperty.call(args, 'todos')) {
    return parseFullList(args.todos) ?? [...current];
  }

  if (args.action === 'add') {
    const content = typeof args.content === 'string' ? args.content.trim() : '';
    return content ? [...current, { content, status: 'pending' }] : [...current];
  }

  if (args.action === 'update') {
    const id = typeof args.id === 'number' && Number.isInteger(args.id) ? args.id : 0;
    const status = parseStatus(args.status);
    if (id < 1 || id > current.length || status === null) return [...current];
    return current.map((item, index) => {
      if (index === id - 1) return { ...item, status };
      if (status === 'in_progress' && item.status === 'in_progress') {
        return { ...item, status: 'pending' };
      }
      return { ...item };
    });
  }

  return [...current];
}

/**
 * Rebuild a parallel tool batch in call order, independent of result arrival order.
 * Successful calls advance committed state; unresolved calls are preview-only and
 * failed calls have no effect. This mirrors the TUI's staged TodoWrite projection.
 */
export function projectTodoCalls(
  base: readonly TodoItem[],
  calls: readonly TodoProjectionCall[],
): TodoProjection {
  let committed = [...base];
  let preview = [...base];
  let hasUnresolved = false;

  for (const call of calls) {
    if (call.success === false) continue;
    if (call.success === true) {
      committed = applyTodoCall(committed, call.name, call.args);
      preview = applyTodoCall(preview, call.name, call.args);
    } else {
      hasUnresolved = true;
      preview = applyTodoCall(preview, call.name, call.args);
    }
  }

  return { committed, preview, hasUnresolved };
}
