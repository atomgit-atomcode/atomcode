import { useEffect, useRef, useState } from "preact/hooks";
import { useSettings } from "../settings";
import { GitDiff, isDeleted, isNotRepository, readChanges, type Change } from "./WorkbenchViewer";
export function WorkbenchGit({
  session,
  version,
  onOpen,
  maximized,
  onMaximize,
  onRestore,
  requested,
}: {
  session: string;
  version: number;
  onOpen: (path: string) => void;
  maximized: boolean;
  onMaximize: () => void;
  onRestore: () => void;
  requested?: { path: string; seq: number };
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [changes, setChanges] = useState<Change[]>([]);
  const [selected, setSelected] = useState<Change | null>(null);
  useEffect(() => { if (!selected) onRestore(); }, [selected]);
  const [staged, setStaged] = useState(false);
  const [error, setError] = useState("");
  const [notRepository, setNotRepository] = useState(false);
  const [loading, setLoading] = useState(true);
  const appliedRequest = useRef<number>();
  useEffect(() => {
    let cancelled = false;
    setError("");
    setLoading(true);
    setNotRepository(false);
    readChanges(session)
      .then((rows) => {
        if (!cancelled) {
          setChanges(rows);
          if (requested && appliedRequest.current !== requested.seq) {
            appliedRequest.current = requested.seq;
            const target = rows.find(row => row.path === requested.path) ?? null;
            setSelected(target);
            setStaged(target?.worktree === " ");
          } else {
            setSelected((old) => old ? rows.find((r) => r.path === old.path) || null : null);
          }
        }
      })
      .catch((e) => {
        if (!cancelled) {
          const absent = isNotRepository(e);
          setNotRepository(absent);
          setError(absent ? "" : String(e));
          setChanges([]);
          setSelected(null);
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [session, version, requested?.seq]);
  return (
    <div class="workbench-git">
      <div class="workbench-git-caption">
        {zh
          ? "当前 Git 变更（只读）· 仓库此刻的状态，不执行 Git 写操作"
          : "Current Git changes (read-only) · the repository as it is now; no Git write actions"}
      </div>
      {error && (
        <p role="alert" class="workbench-error">
          {error}
        </p>
      )}
      <div
        class="workbench-changes"
        aria-label={zh ? "Git 变更文件" : "Git changed files"}
      >
        {changes.map((change) => (
          <button
            key={change.path}
            class={selected?.path === change.path ? "active" : ""}
            onClick={() => {
              setSelected(change);
              setStaged(change.worktree === " ");
            }}
          >
            <code>
              {change.index}
              {change.worktree}
            </code>
            <span>{change.path}</span>
          </button>
        ))}
      </div>
      {!changes.length && !error && (
        <div class="workbench-empty">
          {loading
            ? zh
              ? "正在读取变更…"
              : "Loading changes…"
            : notRepository
              ? zh
                ? "当前目录尚未初始化 Git，暂无版本变更"
                : "This directory is not a Git repository. No version changes to show."
            : zh
              ? "工作区没有文件变更"
              : "No changed files"}
        </div>
      )}
      {selected && (
        <GitDiff session={session} change={selected} version={version}
          staged={staged} onStaged={setStaged}>
          <button
            aria-label={zh ? (maximized ? "恢复差异布局" : "最大化差异") : (maximized ? "Restore diff layout" : "Maximize diff")}
            title={zh ? "最大化 / 恢复（Esc）" : "Maximize / restore (Esc)"}
            aria-pressed={maximized}
            onClick={onMaximize}
          >{maximized ? "⤡" : "⤢"}</button>
          {!isDeleted(selected) && (
            <button onClick={() => onOpen(selected.path)}>
              {zh ? "打开文件" : "Open file"}
            </button>
          )}
        </GitDiff>
      )}
    </div>
  );
}
