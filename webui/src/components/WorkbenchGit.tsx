import { useEffect, useState } from "preact/hooks";
import { workbenchRequest } from "../api";
import { useSettings } from "../settings";
type Change = {
  path: string;
  original?: string;
  index: string;
  worktree: string;
};
export function WorkbenchGit({
  session,
  version,
  onOpen,
}: {
  session: string;
  version: number;
  onOpen: (path: string) => void;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [changes, setChanges] = useState<Change[]>([]);
  const [selected, setSelected] = useState<Change | null>(null);
  const [staged, setStaged] = useState(false);
  const [diff, setDiff] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    let cancelled = false;
    setError("");
    setLoading(true);
    workbenchRequest<Change[]>(
      "git?" + new URLSearchParams({ session_id: session }),
    )
      .then((rows) => {
        if (!cancelled) {
          setChanges(rows);
          setSelected((old) =>
            old ? rows.find((r) => r.path === old.path) || null : null,
          );
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setError(String(e));
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
  }, [session, version]);
  useEffect(() => {
    if (!selected) {
      setDiff("");
      return;
    }
    let cancelled = false;
    setError("");
    setDiff("");
    const useIndex = staged && selected.index !== " " && selected.index !== "?";
    workbenchRequest<{ diff: string }>(
      "diff?" +
        new URLSearchParams({
          session_id: session,
          path: selected.path,
          staged: String(useIndex),
          untracked: String(selected.index === "?"),
        }),
    )
      .then((result) => {
        if (!cancelled) setDiff(result.diff);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [session, selected, staged, version]);
  return (
    <div class="workbench-git">
      <div class="workbench-git-caption">
        {zh
          ? "只读变更 · 不执行 Git 写操作"
          : "Read-only changes · no Git write actions"}
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
            : zh
              ? "工作区没有文件变更"
              : "No changed files"}
        </div>
      )}
      {selected && (
        <>
          <div class="workbench-diff-actions">
            <button
              class={!staged ? "active" : ""}
              onClick={() => setStaged(false)}
            >
              {zh ? "工作区" : "Working tree"}
            </button>
            {selected.index !== " " && selected.index !== "?" && (
              <button
                class={staged ? "active" : ""}
                onClick={() => setStaged(true)}
              >
                {zh ? "暂存区" : "Staged"}
              </button>
            )}
            <span />
            {selected.worktree !== "D" && selected.index !== "D" && (
              <button onClick={() => onOpen(selected.path)}>
                {zh ? "打开文件" : "Open file"}
              </button>
            )}
          </div>
          <pre
            class="workbench-diff"
            aria-label={zh ? "Git 差异" : "Git diff"}
            tabIndex={0}
          >
            {diff
              ? diff.split("\n").map((line, index) => (
                  <span
                    key={index}
                    class={
                      line.startsWith("+")
                        ? "diff-added"
                        : line.startsWith("-")
                          ? "diff-removed"
                          : line.startsWith("@@")
                            ? "diff-hunk"
                            : ""
                    }
                  >
                    {line || " "}
                  </span>
                ))
              : zh
                ? "此视图没有差异"
                : "No diff in this view"}
          </pre>
        </>
      )}
    </div>
  );
}
