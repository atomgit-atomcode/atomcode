// Task 15b — Working directory picker modal

import { useEffect, useRef, useState } from 'preact/hooks';
import { listDir, getProjects, setDefaultDir, ProjectInfo } from '../api';

interface CwdPickerProps {
  current: string;
  onPick: (path: string) => void;
  onClose: () => void;
}

function parseBreadcrumb(path: string): { label: string; fullPath: string }[] {
  // Normalize: replace home dir with ~
  const crumbs: { label: string; fullPath: string }[] = [];
  const parts = path.replace(/\/+$/, '').split('/');

  // If path starts with ~, first crumb is ~
  if (path.startsWith('~')) {
    crumbs.push({ label: '~', fullPath: '~' });
    for (let i = 1; i < parts.length; i++) {
      if (parts[i]) {
        const soFar = parts.slice(0, i + 1).join('/');
        crumbs.push({ label: parts[i], fullPath: soFar });
      }
    }
    return crumbs;
  }

  // Absolute path
  if (path.startsWith('/')) {
    crumbs.push({ label: '/', fullPath: '/' });
    for (let i = 1; i < parts.length; i++) {
      if (parts[i]) {
        const soFar = '/' + parts.slice(1, i + 1).join('/');
        crumbs.push({ label: parts[i], fullPath: soFar });
      }
    }
    return crumbs;
  }

  // Fallback
  crumbs.push({ label: path, fullPath: path });
  return crumbs;
}

export function CwdPicker({ current, onPick, onClose }: CwdPickerProps) {
  const [inputPath, setInputPath] = useState(current || '~');
  const [browsePath, setBrowsePath] = useState(current || '~');
  const [dirs, setDirs] = useState<string[]>([]);
  const [dirLoading, setDirLoading] = useState(false);
  const [dirError, setDirError] = useState<string | null>(null);
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [setAsDefault, setSetAsDefault] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // Load directory listing when browsePath changes
  useEffect(() => {
    setDirLoading(true);
    setDirError(null);
    listDir(browsePath)
      .then((result) => {
        setBrowsePath(result.path); // server may normalize the path
        setDirs(result.dirs);
      })
      .catch((e: unknown) => {
        setDirError(e instanceof Error ? e.message : String(e));
        setDirs([]);
      })
      .finally(() => setDirLoading(false));
  }, [browsePath]);

  // Load recent projects once
  useEffect(() => {
    getProjects()
      .then(setProjects)
      .catch(() => setProjects([]));
  }, []);

  function handleJump() {
    const p = inputPath.trim();
    if (p) setBrowsePath(p);
  }

  function handleSubdirClick(dirName: string) {
    const newPath = browsePath.replace(/\/+$/, '') + '/' + dirName;
    setBrowsePath(newPath);
    setInputPath(newPath);
  }

  function handleBreadcrumbClick(fullPath: string) {
    setBrowsePath(fullPath);
    setInputPath(fullPath);
  }

  function handleProjectClick(workingDir: string) {
    setBrowsePath(workingDir);
    setInputPath(workingDir);
  }

  async function handleConfirm() {
    const finalPath = browsePath.trim() || inputPath.trim();
    if (!finalPath) return;
    setConfirming(true);
    try {
      if (setAsDefault) {
        await setDefaultDir(finalPath);
      }
      onPick(finalPath);
      onClose();
    } catch {
      // Best-effort; still switch cwd locally
      onPick(finalPath);
      onClose();
    } finally {
      setConfirming(false);
    }
  }

  const crumbs = parseBreadcrumb(browsePath);

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="modal-card">
        <div class="modal-header">
          <span>📁</span>
          <h3>切换工作目录</h3>
          <span class="modal-sub" style="margin-left:auto">仅影响当前会话</span>
        </div>

        <div class="modal-body">
          {/* Path input */}
          <div class="field-group">
            <span class="modal-label">路径</span>
            <div class="field-row">
              <input
                ref={inputRef}
                type="text"
                class="menu-input"
                value={inputPath}
                onInput={(e) => setInputPath((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') handleJump();
                }}
                placeholder="~/..."
              />
              <button class="btn btn-primary" onClick={handleJump}>
                跳转
              </button>
            </div>
            <p class="field-hint">
              支持 <code>~</code> 展开。下方可浏览子目录（GET /fs/list）。
            </p>
          </div>

          {/* Directory browser */}
          <div class="dir-browser">
            <div class="dir-breadcrumb">
              {crumbs.map((crumb, i) => (
                <span key={i}>
                  <span
                    onClick={() => handleBreadcrumbClick(crumb.fullPath)}
                    class={'dir-crumb' + (i === crumbs.length - 1 ? ' current' : '')}
                  >
                    {crumb.label}
                  </span>
                  {i < crumbs.length - 1 && <span> / </span>}
                </span>
              ))}
            </div>

            <div class="dir-list">
              {dirLoading && <div class="dir-note">加载中…</div>}
              {dirError && <div class="dir-note error">{dirError}</div>}
              {!dirLoading && !dirError && dirs.length === 0 && (
                <div class="dir-note">（无子目录）</div>
              )}
              {!dirLoading &&
                dirs.map((d) => (
                  <button key={d} class="dir-item" onClick={() => handleSubdirClick(d)}>
                    <span>📁</span>
                    <span>{d}</span>
                  </button>
                ))}
            </div>
          </div>

          {/* Recent projects */}
          {projects.length > 0 && (
            <div class="field-group">
              <span class="modal-label">最近项目</span>
              {projects.slice(0, 6).map((p) => {
                const isCurrent = p.working_dir === browsePath;
                return (
                  <button
                    key={p.hash}
                    class={'list-row' + (isCurrent ? ' active' : '')}
                    onClick={() => handleProjectClick(p.working_dir)}
                  >
                    <span class="mono">{p.working_dir}</span>
                    {isCurrent && <span class="badge">● 当前</span>}
                  </button>
                );
              })}
            </div>
          )}
        </div>

        <div class="modal-footer">
          <label class="checkbox-label">
            <input
              type="checkbox"
              checked={setAsDefault}
              onChange={(e) =>
                setSetAsDefault((e.target as HTMLInputElement).checked)
              }
            />
            同时设为 daemon 默认目录（POST /cd）
          </label>
          <button class="btn" onClick={onClose}>
            取消
          </button>
          <button class="btn btn-primary" onClick={handleConfirm} disabled={confirming}>
            确定
          </button>
        </div>
      </div>
    </div>
  );
}
