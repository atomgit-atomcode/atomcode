export function formatTokenCount(total: number): string {
  if (total < 1000) return `${total} tokens`;
  return `${(total / 1000).toFixed(1)}k tokens`;
}

export function formatTimeAgo(dateStr?: string): string {
  if (!dateStr) return '';
  const diff = Date.now() - new Date(dateStr).getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return 'older';
}

export function escapeHtml(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

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

export function formatToolArgs(name: string, argsJson: string): string {
  try {
    const args = JSON.parse(argsJson) as Record<string, string>;
    if (name === 'read_file' || name === 'Read') return args.file_path ?? args.path ?? '';
    if (name === 'write_file' || name === 'Write') return args.file_path ?? args.path ?? '';
    if (name === 'edit_file' || name === 'Edit') return args.file_path ?? args.path ?? '';
    if (name === 'bash' || name === 'Bash') return (args.command ?? '').substring(0, 80);
    if (name === 'grep' || name === 'Grep') return `${args.pattern ?? ''} in ${args.path ?? '.'}`;
    if (name === 'list_dir') return args.path ?? '.';
    return '';
  } catch {
    return '';
  }
}
