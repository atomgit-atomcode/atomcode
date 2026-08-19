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
  hasApplicable: boolean;
}

export type TodoPanelEvent =
  | { type: 'user_input' }
  | { type: 'todo_call'; success?: boolean }
  | { type: 'user_cancel' };

/** Visibility is presentation-only: a new instruction hides the prior turn's
 * card without deleting its todo state; a valid/pending TodoWrite reveals it. */
export function reduceTodoPanelVisibility(
  visible: boolean,
  event: TodoPanelEvent,
): boolean {
  switch (event.type) {
    case 'user_input':
    case 'user_cancel':
      return false;
    case 'todo_call':
      return event.success === false ? visible : true;
  }
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
  return applyTodoCallResult(current, name, argsJson).items;
}

function applyTodoCallResult(
  current: readonly TodoItem[],
  name: string,
  argsJson: string,
): { items: TodoItem[]; applicable: boolean } {
  if (!isTodoTool(name)) return { items: [...current], applicable: false };
  const args = parseArgs(argsJson);
  if (!args) return { items: [...current], applicable: false };

  if (Object.prototype.hasOwnProperty.call(args, 'todos')) {
    const items = parseFullList(args.todos);
    return items === null
      ? { items: [...current], applicable: false }
      : { items, applicable: true };
  }

  if (args.action === 'add') {
    const content = typeof args.content === 'string' ? args.content.trim() : '';
    return content
      ? { items: [...current, { content, status: 'pending' }], applicable: true }
      : { items: [...current], applicable: false };
  }

  if (args.action === 'update') {
    const id = typeof args.id === 'number' && Number.isInteger(args.id) ? args.id : 0;
    const status = parseStatus(args.status);
    if (id < 1 || id > current.length || status === null) {
      return { items: [...current], applicable: false };
    }
    const items: TodoItem[] = current.map((item, index) => {
      if (index === id - 1) return { ...item, status };
      if (status === 'in_progress' && item.status === 'in_progress') {
        return { ...item, status: 'pending' };
      }
      return { ...item };
    });
    return { items, applicable: true };
  }

  return { items: [...current], applicable: false };
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
  let hasApplicable = false;

  for (const call of calls) {
    if (call.success === false) continue;
    if (call.success === true) {
      const nextCommitted = applyTodoCallResult(committed, call.name, call.args);
      const nextPreview = applyTodoCallResult(preview, call.name, call.args);
      if (!nextCommitted.applicable && !nextPreview.applicable) continue;
      hasApplicable = true;
      if (nextCommitted.applicable) committed = nextCommitted.items;
      if (nextPreview.applicable) preview = nextPreview.items;
    } else {
      const nextPreview = applyTodoCallResult(preview, call.name, call.args);
      if (!nextPreview.applicable) continue;
      hasApplicable = true;
      hasUnresolved = true;
      preview = nextPreview.items;
    }
  }

  return { committed, preview, hasUnresolved, hasApplicable };
}
