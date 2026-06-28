import { useEffect, useRef, useState } from 'react';
import { getFsTree, FsTreeEntry, readFile } from '../api/daemonClient';

interface FileExplorerProps {
  apiBaseUrl: string;
  cwd: string;
  onOpenFile: (path: string) => void;
  onOpenFolder: () => void;
}

function FolderIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" opacity="0.7">
      <path d="M2 4.5A1.5 1.5 0 013.5 3h2.879a1.5 1.5 0 011.06.44l1.122 1.12A1.5 1.5 0 009.62 5H12.5A1.5 1.5 0 0114 6.5v5a1.5 1.5 0 01-1.5 1.5h-9A1.5 1.5 0 012 11.5v-7z" />
    </svg>
  );
}

function FileIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" opacity="0.55">
      <path d="M3 1.5A1.5 1.5 0 004.5 3h5.879a1.5 1.5 0 011.06.44l2.122 2.12a1.5 1.5 0 01.439 1.061V13.5A1.5 1.5 0 0112.5 15h-8A1.5 1.5 0 013 13.5v-12z" />
    </svg>
  );
}

function Chevron({ open }: { open: boolean }) {
  return (
    <svg
      width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5"
      style={{ transform: open ? 'rotate(90deg)' : 'rotate(0deg)', transition: 'transform 0.1s' }}
    >
      <path d="M6 4l4 4-4 4" />
    </svg>
  );
}

interface TreeNodeProps {
  entry: FsTreeEntry;
  depth: number;
  openDirs: Set<string>;
  onToggle: (path: string) => void;
  onOpenFile: (path: string) => void;
}

function TreeNode({ entry, depth, openDirs, onToggle, onOpenFile }: TreeNodeProps) {
  const isDir = entry.type === 'directory';
  const isOpen = openDirs.has(entry.path);

  if (entry.type === 'limit') {
    return (
      <div className="fe-row fe-limit" style={{ paddingLeft: `${8 + depth * 16}px` }}>
        <span className="fe-item-name fe-limit-text">{entry.name}</span>
      </div>
    );
  }

  return (
    <>
      <button
        className={'fe-row' + (isDir ? ' fe-dir' : ' fe-file')}
        style={{ paddingLeft: `${8 + depth * 16}px` }}
        onClick={() => isDir ? onToggle(entry.path) : onOpenFile(entry.path)}
        title={entry.path}
      >
        {isDir && <span className="fe-chevron"><Chevron open={isOpen} /></span>}
        {!isDir && <span className="fe-spacer" />}
        <span className="fe-icon">{isDir ? <FolderIcon /> : <FileIcon />}</span>
        <span className="fe-item-name">{entry.name}</span>
      </button>
      {isDir && isOpen && entry.children?.map((child) => (
        <TreeNode
          key={child.path}
          entry={child}
          depth={depth + 1}
          openDirs={openDirs}
          onToggle={onToggle}
          onOpenFile={onOpenFile}
        />
      ))}
    </>
  );
}

export function FileExplorer({ apiBaseUrl, cwd, onOpenFile, onOpenFolder }: FileExplorerProps) {
  const [tree, setTree] = useState<FsTreeEntry[]>([]);
  const [openDirs, setOpenDirs] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!cwd) return;
    let cancelled = false;
    setLoading(true);
    setError(null);

    getFsTree(apiBaseUrl, cwd)
      .then((data) => {
        if (cancelled) return;
        setTree(data.tree);
        // Auto-open first level
        const autoOpen = new Set<string>();
        data.tree.forEach((entry) => {
          if (entry.type === 'directory') autoOpen.add(entry.path);
        });
        setOpenDirs(autoOpen);
      })
      .catch((err) => {
        if (!cancelled) setError(err.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => { cancelled = true; };
  }, [apiBaseUrl, cwd]);

  function toggleDir(path: string) {
    setOpenDirs((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  const dirName = cwd.split(/[/\\]/).pop() || '';

  return (
    <div className="fe-container">
      <div className="fe-header">
        <span className="fe-header-icon"><FolderIcon /></span>
        <span className="fe-header-title">{dirName || 'No folder open'}</span>
        <button className="fe-header-btn" onClick={onOpenFolder} title="Open Folder">
          +
        </button>
      </div>
      {!cwd ? (
        <div className="fe-empty">
          <p>No folder opened</p>
          <button className="fe-open-btn" onClick={onOpenFolder}>Open Folder</button>
        </div>
      ) : loading ? (
        <div className="fe-status">Loading...</div>
      ) : error ? (
        <div className="fe-status fe-error">{error}</div>
      ) : (
        <div className="fe-tree">
          {tree.map((entry) => (
            <TreeNode
              key={entry.path}
              entry={entry}
              depth={0}
              openDirs={openDirs}
              onToggle={toggleDir}
              onOpenFile={onOpenFile}
            />
          ))}
        </div>
      )}
    </div>
  );
}
