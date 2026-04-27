/** Format a token count for display (e.g. 1234 -> "1.2k") */
export function formatTokenCount(total: number): string {
  if (total < 1000) return `${total} tokens`;
  return `${(total / 1000).toFixed(1)}k tokens`;
}

/** Human-readable time-ago string */
export function formatTimeAgo(dateStr?: string): string {
  if (!dateStr) return '';
  const diff = Date.now() - new Date(dateStr).getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/** Escape HTML entities */
export function escapeHtml(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/** Group sessions by date category */
export function groupSessionsByDate<T extends { updated_at?: string; created_at?: string }>(
  sessions: T[],
): Record<string, T[]> {
  const groups: Record<string, T[]> = {};
  const now = Date.now();
  const oneDay = 86400000;

  sessions.forEach((s) => {
    const ts = new Date(s.updated_at ?? s.created_at ?? now).getTime();
    const diff = now - ts;
    let label: string;
    if (diff < oneDay) label = 'Today';
    else if (diff < 2 * oneDay) label = 'Yesterday';
    else if (diff < 7 * oneDay) label = 'This Week';
    else label = 'Older';
    if (!groups[label]) groups[label] = [];
    groups[label].push(s);
  });
  return groups;
}

/** Get a display-friendly icon for a tool name */
export function getToolIcon(name: string): string {
  const icons: Record<string, string> = {
    read_file: '📄',
    write_file: '✍️',
    edit_file: '✍️',
    bash: '💻',
    grep: '🔍',
    list_dir: '📁',
    search: '🔍',
  };
  return icons[name] ?? '🔧';
}

/** Format tool args into a short summary */
export function formatToolArgs(name: string, argsJson: string): string {
  try {
    const args = JSON.parse(argsJson) as Record<string, string>;
    if (name === 'read_file' || name === 'write_file' || name === 'edit_file') {
      return args.file_path ?? args.path ?? '';
    }
    if (name === 'bash') return (args.command ?? '').substring(0, 80);
    if (name === 'grep') return `${args.pattern ?? ''} in ${args.path ?? '.'}`;
    if (name === 'list_dir') return args.path ?? '.';
    return '';
  } catch {
    return '';
  }
}
