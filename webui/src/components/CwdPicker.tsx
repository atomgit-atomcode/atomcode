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
      class="fixed inset-0 bg-black/40 flex items-center justify-center p-4 z-50"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="bg-white rounded-2xl shadow-2xl max-w-lg w-full overflow-hidden">
        {/* Header */}
        <div class="px-5 py-4 border-b border-slate-200 flex items-center gap-2">
          <span class="text-xl">📁</span>
          <h3 class="font-semibold text-base">切换工作目录</h3>
          <span class="ml-auto text-xs text-slate-400">仅影响当前会话</span>
        </div>

        {/* Body */}
        <div class="px-5 py-4 space-y-4">
          {/* Path input */}
          <div>
            <label class="text-xs font-semibold text-slate-400 uppercase">
              路径
            </label>
            <div class="mt-1 flex gap-2">
              <input
                ref={inputRef}
                type="text"
                value={inputPath}
                onInput={(e) => setInputPath((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') handleJump();
                }}
                class="flex-1 border border-slate-300 rounded-lg px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
                placeholder="~/..."
              />
              <button
                onClick={handleJump}
                class="px-3 py-2 rounded-lg bg-indigo-600 text-white text-sm hover:bg-indigo-700"
              >
                跳转
              </button>
            </div>
            <p class="mt-1 text-xs text-slate-400">
              支持 <code class="font-mono">~</code> 展开。下方可浏览子目录（GET /fs/list）。
            </p>
          </div>

          {/* Directory browser */}
          <div class="border border-slate-200 rounded-lg overflow-hidden">
            {/* Breadcrumb */}
            <div class="px-3 py-2 bg-slate-50 border-b border-slate-200 text-xs font-mono text-slate-500 flex items-center flex-wrap gap-0.5">
              {crumbs.map((crumb, i) => (
                <span key={i} class="flex items-center gap-0.5">
                  <span
                    onClick={() => handleBreadcrumbClick(crumb.fullPath)}
                    class={
                      i === crumbs.length - 1
                        ? 'font-semibold text-slate-700'
                        : 'hover:text-indigo-600 cursor-pointer'
                    }
                  >
                    {crumb.label}
                  </span>
                  {i < crumbs.length - 1 && <span>/</span>}
                </span>
              ))}
            </div>

            {/* Subdirectory list */}
            <div class="max-h-44 overflow-y-auto text-sm">
              {dirLoading && (
                <div class="px-3 py-2 text-xs text-slate-400">加载中…</div>
              )}
              {dirError && (
                <div class="px-3 py-2 text-xs text-rose-500">{dirError}</div>
              )}
              {!dirLoading && !dirError && dirs.length === 0 && (
                <div class="px-3 py-2 text-xs text-slate-400">（无子目录）</div>
              )}
              {!dirLoading &&
                dirs.map((d) => (
                  <div
                    key={d}
                    onClick={() => handleSubdirClick(d)}
                    class="px-3 py-1.5 hover:bg-indigo-50 cursor-pointer flex items-center gap-2"
                  >
                    <span class="text-slate-400">📁</span>
                    <span>{d}</span>
                  </div>
                ))}
            </div>
          </div>

          {/* Recent projects */}
          {projects.length > 0 && (
            <div>
              <label class="text-xs font-semibold text-slate-400 uppercase">
                最近项目
              </label>
              <div class="mt-1 space-y-1">
                {projects.slice(0, 6).map((p) => {
                  const isCurrent = p.working_dir === browsePath;
                  return (
                    <div
                      key={p.hash}
                      onClick={() => handleProjectClick(p.working_dir)}
                      class={
                        'px-3 py-2 rounded-lg cursor-pointer text-sm flex items-center justify-between ' +
                        (isCurrent
                          ? 'bg-indigo-50 border border-indigo-200'
                          : 'hover:bg-slate-50')
                      }
                    >
                      <span class="font-mono truncate">{p.working_dir}</span>
                      {isCurrent && (
                        <span class="text-xs text-emerald-600 shrink-0 ml-2">
                          ● 当前
                        </span>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </div>

        {/* Footer */}
        <div class="px-5 py-3 bg-slate-50 border-t border-slate-200 flex items-center gap-2 justify-end">
          <label class="mr-auto flex items-center gap-1.5 text-xs text-slate-500 cursor-pointer">
            <input
              type="checkbox"
              checked={setAsDefault}
              onChange={(e) =>
                setSetAsDefault((e.target as HTMLInputElement).checked)
              }
            />
            同时设为 daemon 默认目录（POST /cd，影响所有客户端）
          </label>
          <button
            onClick={onClose}
            class="px-3.5 py-1.5 rounded-lg bg-white border border-slate-300 text-sm hover:bg-slate-100"
          >
            取消
          </button>
          <button
            onClick={handleConfirm}
            disabled={confirming}
            class="px-3.5 py-1.5 rounded-lg bg-indigo-600 text-white text-sm hover:bg-indigo-700 disabled:opacity-50"
          >
            确定
          </button>
        </div>
      </div>
    </div>
  );
}
