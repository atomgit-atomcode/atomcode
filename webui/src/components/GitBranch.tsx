import { useEffect, useState } from 'preact/hooks';
import { getGitBranch } from '../api';

/** Current checkout metadata, independently refreshed while the page is visible. */
export function GitBranch({ cwd }: { cwd: string }) {
  const [value, setValue] = useState<{ dir: string; branch: string | null } | null>(null);
  useEffect(() => {
    if (!cwd) return;
    let alive = true;
    let loading = false;
    let pending: AbortController | null = null;
    const refresh = async () => {
      if (document.hidden || loading) return;
      loading = true;
      const controller = new AbortController();
      pending = controller;
      const timeout = window.setTimeout(() => controller.abort(), 5000);
      try {
        const result = await getGitBranch(cwd, controller.signal);
        if (alive) setValue({ dir: cwd, branch: result.branch });
      } catch {
        if (alive) setValue({ dir: cwd, branch: null });
      } finally {
        window.clearTimeout(timeout);
        pending = null;
        loading = false;
      }
    };
    void refresh();
    const interval = window.setInterval(() => { void refresh(); }, 2000);
    const onFocus = () => { void refresh(); };
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onFocus);
    return () => {
      alive = false;
      pending?.abort();
      window.clearInterval(interval);
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onFocus);
    };
  }, [cwd]);
  // A directory change must never flash the previous checkout's branch.
  const branch = value?.dir === cwd ? value.branch : null;
  if (!branch) return null;
  return <span class="input-cwd-branch" title={`Git: ${branch}`}>
    <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="4" cy="3" r="1.5" stroke="currentColor" />
      <circle cx="4" cy="13" r="1.5" stroke="currentColor" />
      <circle cx="12" cy="3" r="1.5" stroke="currentColor" />
      <path d="M4 4.5v7M12 4.5v1c0 3-8 1-8 5" stroke="currentColor" />
    </svg>
    <span class="input-cwd-branch-name">{branch}</span>
  </span>;
}
