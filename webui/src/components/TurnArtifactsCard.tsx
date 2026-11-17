import { createContext, type ComponentChildren } from 'preact';
import { useContext, useEffect, useState } from 'preact/hooks';
import { workbenchRequest } from '../api';
import { artifactDisplayPath, type TurnArtifact } from '../lib/turnArtifacts';
import { useT } from '../settings';

interface WorkspaceValue {
  sessionId: string | null;
  cwd: string;
  viewChanges?: (path: string) => void;
  repository?: boolean;
  stats?: ReadonlyMap<string, { additions: number; deletions: number }>;
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
      setResult({sessionId, cwd, repository: true, stats});
    }).catch(() => {
      if (!stopped) setResult({sessionId, cwd, repository: false});
    });
    return () => { stopped = true; };
  }, [value.sessionId, value.cwd, version]);
  const current = result?.sessionId === value.sessionId && result?.cwd === value.cwd ? result : null;
  return <ArtifactWorkspace.Provider value={{...value, repository: current?.repository, stats: current?.stats}}>{children}</ArtifactWorkspace.Provider>;
}

export function TurnArtifactsCard({ artifacts, sessionId, onPreview }: {
  artifacts: TurnArtifact[];
  sessionId: string | null;
  onPreview: (path: string) => void;
}) {
  const t = useT();
  const workspace = useContext(ArtifactWorkspace);
  const [expanded, setExpanded] = useState(false);
  const scope = workspace?.sessionId === sessionId ? workspace : null;
  return <section class="turn-artifacts" aria-label={t('chat.modifiedFiles', { count: artifacts.length })}>
    <div class="turn-artifacts-header">
      <svg class="turn-artifacts-icon" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
        <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9zM14 3v6h6M8 14h8M8 17h5" />
      </svg>
      <strong>{t('chat.modifiedFiles', { count: artifacts.length })}</strong>
      {scope?.repository && scope.viewChanges && <button type="button" class="turn-artifacts-changes"
        onClick={() => scope.viewChanges?.(artifactDisplayPath(artifacts[0].path, scope.cwd))}>
        {t('chat.viewChanges')}
      </button>}
    </div>
    <ul class="turn-artifacts-list">
      {(expanded ? artifacts : artifacts.slice(0, 3)).map(artifact => {
        const path = artifactDisplayPath(artifact.path, scope?.cwd);
        const stats = scope?.stats?.get(path);
        return <li key={artifact.path}>
        <button type="button" class="turn-artifact-chip" title={artifact.path} onClick={() => onPreview(artifact.path)}>
          <span class="turn-artifact-path">{path}</span>
          {stats && <span class="turn-artifact-stats"><span class="turn-artifact-added">+{stats.additions}</span><span class="turn-artifact-removed">-{stats.deletions}</span></span>}
        </button>
      </li>;
      })}
    </ul>
    {artifacts.length > 3 && <button type="button" class="turn-artifact-more" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      {expanded ? t('chat.collapseFiles') : t('chat.moreFiles', { count: artifacts.length - 3 })}
      <span aria-hidden="true">{expanded ? '⌃' : '⌄'}</span>
    </button>}
  </section>;
}
