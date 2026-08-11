export type TodoTitles = Map<number, string>;

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

function parseTodos(value: unknown): Array<Record<string, unknown>> | null {
  if (typeof value === 'string') {
    try { value = JSON.parse(value); } catch { return null; }
  }
  return Array.isArray(value) ? value as Array<Record<string, unknown>> : null;
}

export function todoToolDetail(
  name: string,
  argsJson: string,
  titles: TodoTitles,
): string | null {
  if (name !== 'todo' && name !== 'todowrite') return null;
  const args = parseArgs(argsJson);
  if (!args) return '';

  const todos = parseTodos(args.todos);
  if (todos) return `${todos.length} ${todos.length === 1 ? 'task' : 'tasks'}`;

  const action = typeof args.action === 'string' ? args.action : '';
  if (action === 'add') return typeof args.content === 'string' ? args.content : '';
  if (action === 'update') {
    const id = typeof args.id === 'number' ? args.id : 0;
    const status = typeof args.status === 'string' ? args.status : '';
    const title = titles.get(id);
    if (!id) return status;
    return [`#${id}`, title, status ? `→ ${status}` : ''].filter(Boolean).join(' ');
  }
  if (action === 'list') return 'list all';
  return '';
}

/** Commit only a successful call, matching the TUI's staged todo semantics. */
export function commitTodoCall(
  name: string,
  argsJson: string,
  titles: TodoTitles,
): void {
  if (name !== 'todo' && name !== 'todowrite') return;
  const args = parseArgs(argsJson);
  if (!args) return;

  const todos = parseTodos(args.todos);
  if (todos) {
    titles.clear();
    todos.forEach((todo, index) => {
      if (typeof todo.content === 'string' && todo.content.trim()) {
        titles.set(index + 1, todo.content.trim());
      }
    });
    return;
  }

  if (args.action === 'add' && typeof args.content === 'string' && args.content.trim()) {
    const nextId = Math.max(0, ...titles.keys()) + 1;
    titles.set(nextId, args.content.trim());
  }
}
