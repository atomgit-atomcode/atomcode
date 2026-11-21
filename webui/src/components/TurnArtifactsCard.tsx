import { createContext, type ComponentChildren } from 'preact';
import { useContext, useEffect, useState } from 'preact/hooks';
import { workbenchRequest } from '../api';
import { FileViewerContext } from '../lib/fileViewer';
import { artifactDisplayPath, resolveArtifactPath, workspaceRelativePath, type TurnArtifact } from '../lib/turnArtifacts';
import { openWithHost } from '../lib/androidHost';
import { useT } from '../settings';

interface WorkspaceValue {
  sessionId: string | null;
  cwd: string;
  repository?: boolean;
  stats?: ReadonlyMap<string, { additions: number; deletions: number }>;
  /** Paths Git currently reports as changed (working tree, not this turn's history). */
  changed?: ReadonlySet<string>;
}
export const ArtifactWorkspace = createContext<WorkspaceValue | null>(null);

/** One Git read shared by every historical card in the displayed session. */
export function ArtifactWorkspaceProvider({ value, version, children }: {
  value: WorkspaceValue;
  version: string;
  children: ComponentChildren;
}) {
  const [result, setResult] = useState<WorkspaceValue | null>(null);
  useEffect(() => {
    const sessionId = value.sessionId;
    const cwd = value.cwd;
    if (!sessionId) return;
    let stopped = false;
    workbenchRequest<{ path: string; stats?: { additions: number; deletions: number } }[]>(
      'git?' + new URLSearchParams({session_id: sessionId, stats: 'true'}),
    ).then(rows => {
      if (stopped) return;
      const stats = new Map(rows.filter(row => row.stats).map(row => [row.path, row.stats!]));
      const changed = new Set(rows.map(row => row.path));
      setResult({sessionId, cwd, repository: true, stats, changed});
    }).catch(() => {
      if (!stopped) setResult({sessionId, cwd, repository: false});
    });
    return () => { stopped = true; };
  }, [value.sessionId, value.cwd, version]);
  const current = result?.sessionId === value.sessionId && result?.cwd === value.cwd ? result : null;
  return <ArtifactWorkspace.Provider value={{...value, repository: current?.repository, stats: current?.stats, changed: current?.changed}}>{children}</ArtifactWorkspace.Provider>;
}

export function TurnArtifactsCard({ artifacts, sessionId, cwd }: {
  artifacts: TurnArtifact[];
  sessionId: string | null;
  /** The workspace root, for the phone's own viewer. */
  cwd?: string;
}) {
  const t = useT();
  const workspace = useContext(ArtifactWorkspace);
  const viewer = useContext(FileViewerContext);
  const [expanded, setExpanded] = useState(false);
  const scope = workspace?.sessionId === sessionId ? workspace : null;
  // Every click goes to the owning session's workspace viewer. A path outside
  // the workspace is still sent as written: the daemon denies it and the
  // viewer shows that error, rather than a second preview surface.
  const route = viewer && sessionId
    ? (path: string, view: 'content' | 'diff') => viewer.open({ sessionId, path, view })
    : null;
  // On the phone the viewer is the app shell's job: a sandboxed daemon process
  // cannot launch one, so `/fs/open` answers with "open the path from the app
  // UI" and nothing happens. Everywhere else there is no bridge, so the
  // workbench pane takes it and the daemon opens the file itself if nothing
  // else does.
  const hostOpen = (path: string): boolean =>
    openWithHost(resolveArtifactPath(path, cwd ?? scope?.cwd ?? ''));
  return <section class="turn-artifacts" aria-label={t('chat.modifiedFiles', { count: artifacts.length })}>
    <div class="turn-artifacts-header">
      <svg class="turn-artifacts-icon" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
        <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9zM14 3v6h6M8 14h8M8 17h5" />
      </svg>
      <strong>{t('chat.modifiedFiles', { count: artifacts.length })}</strong>
      {scope?.repository && route && <button type="button" class="turn-artifacts-changes"
        onClick={() => route(workspaceRelativePath(artifacts[0].path, scope.cwd) ?? artifactDisplayPath(artifacts[0].path, scope.cwd), 'diff')}>
        {t('chat.viewChanges')}
      </button>}
    </div>
    <ul class="turn-artifacts-list">
      {(expanded ? artifacts : artifacts.slice(0, 3)).map(artifact => {
        const path = artifactDisplayPath(artifact.path, scope?.cwd);
        const stats = scope?.stats?.get(path);
        const workspacePath = scope ? workspaceRelativePath(artifact.path, scope.cwd) : null;
        const open = route ? () => route(workspacePath ?? artifact.path, 'content') : undefined;
        const diff = route && scope?.repository && workspacePath && scope.changed?.has(workspacePath)
          ? () => route(workspacePath, 'diff') : null;
        return <li key={artifact.path}>
        <button type="button" class="turn-artifact-chip" title={artifact.path}
          onClick={() => { if (hostOpen(artifact.path)) return; open?.(); }}
          disabled={!open && !cwd}>
          <span class="turn-artifact-path">{path}</span>
          {stats && <span class="turn-artifact-stats"><span class="turn-artifact-added">+{stats.additions}</span><span class="turn-artifact-removed">-{stats.deletions}</span></span>}
        </button>
        {diff && <button type="button" class="turn-artifact-diff" onClick={diff}
          title={t('chat.viewFileDiffTitle', { path })} aria-label={t('chat.viewFileDiffTitle', { path })}>
          {t('chat.viewFileDiff')}
        </button>}
      </li>;
      })}
    </ul>
    {artifacts.length > 3 && <button type="button" class="turn-artifact-more" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      {expanded ? t('chat.collapseFiles') : t('chat.moreFiles', { count: artifacts.length - 3 })}
      <span aria-hidden="true">{expanded ? '⌃' : '⌄'}</span>
    </button>}
  </section>;
}
