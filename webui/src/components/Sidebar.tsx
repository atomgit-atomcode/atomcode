// Task 15 — Session sidebar

import { useEffect, useState } from 'preact/hooks';
import { listSessions, SessionMetaWithProject } from '../api';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
}

function baseName(p: string): string {
  // Return last path component, stripping trailing slashes
  const clean = p.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  return idx >= 0 ? clean.slice(idx + 1) : clean;
}

function shortDir(p: string): string {
  // Replace home dir with ~
  if (p.startsWith('/Users/') || p.startsWith('/home/')) {
    const parts = p.split('/');
    // /Users/<name>/... → ~/...
    if (parts.length >= 3) {
      return '~/' + parts.slice(3).join('/');
    }
  }
  return p;
}

export function Sidebar({ activeSessionId, onSelect, onNew }: SidebarProps) {
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    setLoading(true);
    listSessions()
      .then(setSessions)
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }, []);

  return (
    <aside class="w-64 shrink-0 border-r border-slate-200 bg-white flex flex-col">
      {/* New session button */}
      <div class="p-3">
        <button
          onClick={onNew}
          class="w-full py-2 rounded-lg bg-indigo-600 text-white text-sm font-medium hover:bg-indigo-700"
        >
          ＋ 新建会话
        </button>
      </div>

      {/* Section label */}
      <div class="px-3 pb-1 text-xs font-semibold text-slate-400 uppercase tracking-wide">
        最近会话
      </div>

      {/* Session list */}
      <div class="flex-1 overflow-y-auto px-2 space-y-1 pb-2">
        {loading && (
          <div class="px-3 py-4 text-xs text-slate-400 text-center">加载中…</div>
        )}
        {!loading && sessions.length === 0 && (
          <div class="px-3 py-4 text-xs text-slate-400 text-center">暂无会话</div>
        )}
        {sessions.map((s) => {
          const active = s.id === activeSessionId;
          const label = s.name || s.id.slice(0, 8);
          const dir = shortDir(s.working_dir);
          const dirBase = baseName(s.working_dir);
          return (
            <div
              key={s.id}
              onClick={() => onSelect(s)}
              class={
                'px-3 py-2 rounded-lg cursor-pointer ' +
                (active
                  ? 'bg-indigo-50 border border-indigo-200'
                  : 'hover:bg-slate-50')
              }
            >
              <div class={`text-sm truncate ${active ? 'font-medium' : ''}`}>
                {label}
              </div>
              <div class="text-xs text-slate-400 truncate" title={dir}>
                {dirBase} · {s.message_count} 条
              </div>
            </div>
          );
        })}
      </div>

      {/* Footer */}
      <div class="p-3 border-t border-slate-200 text-xs text-slate-400">
        {sessions.length > 0 ? `${sessions.length} 个会话` : '无会话记录'}
      </div>
    </aside>
  );
}
