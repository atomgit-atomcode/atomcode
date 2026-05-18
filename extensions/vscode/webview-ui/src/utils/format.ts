export function formatTokenCount(total: number): string {
  if (total < 1000) return `${total} tokens`;
  return `${(total / 1000).toFixed(1)}k tokens`;
}

function toTimestamp(value?: string | number): number | undefined {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'number') return value < 10_000_000_000 ? value * 1000 : value;
  const numeric = Number(value);
  if (Number.isFinite(numeric)) return numeric < 10_000_000_000 ? numeric * 1000 : numeric;
  const parsed = new Date(value).getTime();
  return Number.isFinite(parsed) ? parsed : undefined;
}

export function formatTimeAgo(dateStr?: string | number): string {
  const ts = toTimestamp(dateStr);
  if (!ts) return '';
  const diff = Date.now() - ts;
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

export function groupSessionsByDate<T extends { updated_at?: string | number; created_at?: string | number }>(
  sessions: T[],
): Record<string, T[]> {
  const groups: Record<string, T[]> = {};
  const now = Date.now();
  const oneDay = 86400000;
  sessions.forEach((s) => {
    const ts = toTimestamp(s.updated_at ?? s.created_at) ?? now;
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
