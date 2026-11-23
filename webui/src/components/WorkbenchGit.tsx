import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import {
  commitGitStaged,
  discardGitPaths,
  readGitBranches,
  readGitCommit,
  readGitLog,
  stageGitPaths,
  switchGitBranch,
  unstageGitPaths,
  type GitBranchList,
  type GitCommitDetail,
  type GitLogEntry,
} from "../api";
import {
  commitMessageProblem,
  commitSubject,
  groupByDirectory,
  partitionChanges,
  relativeTime,
  selectionsOf,
  statusKind,
  statusLetter,
  sumStats,
  type GitEntry,
} from "../lib/gitPanel";
import { useSettings } from "../settings";
import { ConfirmDialog } from "./ConfirmDialog";
import { GitDiff, isDeleted, isNotRepository, readChanges, type Change } from "./WorkbenchViewer";

type Selection =
  | { kind: "file"; path: string; staged: boolean }
  | { kind: "commit"; hash: string };

/** VSCode-style source control: commit box, staged/changes sections, history and
 *  the diff of whatever is selected. Every write goes through the daemon's
 *  `/workbench/git/*` endpoints; nothing here touches the agent runtime. */
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
  const text = (zhText: string, enText: string) => (zh ? zhText : enText);
  const [changes, setChanges] = useState<Change[]>([]);
  const [branches, setBranches] = useState<GitBranchList | null>(null);
  const [history, setHistory] = useState<GitLogEntry[]>([]);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [detail, setDetail] = useState<GitCommitDetail | null>(null);
  const [detailError, setDetailError] = useState("");
  const [message, setMessage] = useState("");
  const [amend, setAmend] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [notRepository, setNotRepository] = useState(false);
  const [loading, setLoading] = useState(true);
  const [refresh, setRefresh] = useState(0);
  const [discarding, setDiscarding] = useState<GitEntry[] | null>(null);
  const [branchMenu, setBranchMenu] = useState(false);
  const [newBranch, setNewBranch] = useState("");
  const appliedRequest = useRef<number>();
  const commitHash = selection?.kind === "commit" ? selection.hash : "";

  useEffect(() => {
    let cancelled = false;
    setError("");
    setLoading(true);
    setNotRepository(false);
    Promise.all([readChanges(session, true), readGitBranches(session), readGitLog(session)])
      .then(([rows, list, commits]) => {
        if (cancelled) return;
        setChanges(Array.isArray(rows) ? rows : []);
        setBranches(list);
        setHistory(Array.isArray(commits) ? commits : []);
        if (requested && appliedRequest.current !== requested.seq) {
          appliedRequest.current = requested.seq;
          const target = rows.find((row) => row.path === requested.path);
          setSelection(
            target ? { kind: "file", path: target.path, staged: target.worktree === " " } : null,
          );
          return;
        }
        setSelection((old) => {
          if (!old) return old;
          if (old.kind === "commit") {
            return commits.some((commit) => commit.hash === old.hash) ? old : null;
          }
          return rows.some((row) => row.path === old.path) ? old : null;
        });
      })
      .catch((e) => {
        if (cancelled) return;
        const absent = isNotRepository(e);
        setNotRepository(absent);
        setError(absent ? "" : String(e));
        setChanges([]);
        setBranches(null);
        setHistory([]);
        setSelection(null);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [session, version, refresh, requested?.seq]);

  useEffect(() => {
    if (!commitHash) {
      setDetail(null);
      setDetailError("");
      return;
    }
    let cancelled = false;
    setDetail(null);
    setDetailError("");
    readGitCommit(session, commitHash)
      .then((commit) => {
        if (!cancelled) setDetail(commit);
      })
      .catch((e) => {
        if (!cancelled) setDetailError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [session, commitHash, refresh, version]);

  useEffect(() => {
    if (!selection) onRestore();
  }, [selection]);

  const { staged, unstaged } = partitionChanges(changes);
  const problem = commitMessageProblem(message);
  const canCommit = !busy && staged.length > 0 && problem === null;
  const selectedRow =
    selection?.kind === "file" ? changes.find((row) => row.path === selection.path) : undefined;

  async function run(action: () => Promise<unknown>, done: string) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await action();
      setNotice(done);
      setRefresh((value) => value + 1);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function submitCommit() {
    if (!canCommit) return;
    setBusy(true);
    setError("");
    setNotice("");
    commitGitStaged(session, message, amend)
      .then((result) => {
        setMessage("");
        setAmend(false);
        setNotice(
          result.hash
            ? text(`已提交 ${result.hash}`, `Committed ${result.hash}`)
            : text("已提交", "Committed"),
        );
        setRefresh((value) => value + 1);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  }

  function switchTo(name: string, create: boolean) {
    void run(async () => {
      await switchGitBranch(session, name, create);
      setBranchMenu(false);
      setNewBranch("");
      setSelection(null);
    }, text(`已切换到 ${name}`, `Switched to ${name}`));
  }

  function stageOne(entry: GitEntry) {
    void run(
      () => stageGitPaths(session, selectionsOf([entry])),
      text(`已暂存 ${entry.name}`, `Staged ${entry.name}`),
    );
  }
  function unstageOne(entry: GitEntry) {
    void run(
      () => unstageGitPaths(session, selectionsOf([entry])),
      text(`已取消暂存 ${entry.name}`, `Unstaged ${entry.name}`),
    );
  }
  function stageEverything() {
    void run(
      () => stageGitPaths(session, [], true),
      text("已暂存全部更改", "Staged all changes"),
    );
  }
  function unstageEverything() {
    void run(
      () => unstageGitPaths(session, [], true),
      text("已取消全部暂存", "Unstaged everything"),
    );
  }

  const now = Math.floor(Date.now() / 1000);
  const nothingToShow = !changes.length && !history.length && !error;

  return (
    <div class="workbench-git">
      <div class="workbench-git-toolbar">
        <button
          class="git-branch-button"
          aria-expanded={branchMenu}
          title={text("切换或创建分支", "Switch or create a branch")}
          onClick={() => setBranchMenu((open) => !open)}
        >
          <span class="git-branch-icon" aria-hidden="true">
            ⑂
          </span>
          <span class="git-branch-name">
            {branches?.current ?? text("游离 HEAD", "Detached HEAD")}
          </span>
          <span class="git-branch-caret" aria-hidden="true">
            ▾
          </span>
        </button>
        <span class="git-toolbar-count">
          {changes.length ? text(`${changes.length} 个变更`, `${changes.length} changed`) : ""}
        </span>
        <button
          class="git-refresh"
          disabled={busy}
          title={text("刷新", "Refresh")}
          aria-label={text("刷新 Git 状态", "Refresh Git status")}
          onClick={() => setRefresh((value) => value + 1)}
        >
          ↻
        </button>
      </div>

      {branchMenu && (
        <div class="workbench-git-branches" role="group" aria-label={text("分支", "Branches")}>
          <div class="git-branch-create">
            <input
              value={newBranch}
              disabled={busy}
              placeholder={text("新分支名", "New branch name")}
              aria-label={text("新分支名", "New branch name")}
              onInput={(event) => setNewBranch((event.target as HTMLInputElement).value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && newBranch.trim() && !busy) {
                  switchTo(newBranch.trim(), true);
                }
              }}
            />
            <button disabled={busy || !newBranch.trim()} onClick={() => switchTo(newBranch.trim(), true)}>
              {text("创建并切换", "Create & switch")}
            </button>
          </div>
          <div class="git-branch-list">
            {(branches?.branches ?? []).map((branch) => (
              <button
                key={branch.name}
                class={branch.current ? "active" : ""}
                aria-pressed={branch.current}
                disabled={busy || branch.current}
                onClick={() => switchTo(branch.name, false)}
              >
                <span class="git-branch-tick" aria-hidden="true">
                  {branch.current ? "✓" : ""}
                </span>
                <span class="git-branch-trim" title={branch.name}>
                  {branch.name}
                </span>
              </button>
            ))}
            {!(branches?.branches ?? []).length && (
              <p class="git-section-empty">{text("还没有分支", "No branches yet")}</p>
            )}
          </div>
        </div>
      )}

      {notice && (
        <p class="workbench-git-notice" role="status">
          {notice}
        </p>
      )}
      {error && (
        <p role="alert" class="workbench-error">
          {error}
        </p>
      )}

      <div class="workbench-git-body">
        <div class="workbench-git-list" aria-label={text("源控制", "Source control")}>
          <section class="git-commit-box">
            <textarea
              rows={3}
              value={message}
              disabled={busy}
              placeholder={text("提交信息（Ctrl+Enter 提交）", "Commit message (Ctrl+Enter to commit)")}
              aria-label={text("提交信息", "Commit message")}
              onInput={(event) => setMessage((event.target as HTMLTextAreaElement).value)}
              onKeyDown={(event) => {
                if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
                  event.preventDefault();
                  submitCommit();
                }
              }}
            />
            <div class="git-commit-actions">
              <button class="git-commit-button" disabled={!canCommit} onClick={submitCommit}>
                {busy
                  ? text("提交中…", "Committing…")
                  : amend
                    ? text("修补提交", "Amend")
                    : text("提交", "Commit")}
              </button>
              <label class="git-amend" title={text("替换上一次提交", "Replace the last commit")}>
                <input
                  type="checkbox"
                  checked={amend}
                  disabled={busy || !history.length}
                  onChange={(event) => setAmend((event.target as HTMLInputElement).checked)}
                />
                {text("修补", "Amend")}
              </label>
              <span class="git-commit-hint">
                {problem === "empty"
                  ? text("需要提交信息", "Message required")
                  : problem === "too-long"
                    ? text("提交信息过长", "Message too long")
                    : staged.length
                      ? text(`将提交 ${staged.length} 个文件`, `${staged.length} staged`)
                      : text("先暂存再提交", "Stage files to commit")}
              </span>
            </div>
          </section>

          <ChangeSection
            title={text("已暂存", "Staged")}
            entries={staged}
            staged
            busy={busy}
            zh={zh}
            selected={selection}
            onSelect={(entry) =>
              setSelection({ kind: "file", path: entry.path, staged: true })
            }
            onOpen={onOpen}
            onStage={stageOne}
            onUnstage={unstageOne}
            onDiscard={(entry) => setDiscarding([entry])}
            onAll={unstageEverything}
            allLabel={text("全部取消暂存", "Unstage all")}
          />

          <ChangeSection
            title={text("更改", "Changes")}
            entries={unstaged}
            staged={false}
            busy={busy}
            zh={zh}
            selected={selection}
            onSelect={(entry) =>
              setSelection({ kind: "file", path: entry.path, staged: false })
            }
            onOpen={onOpen}
            onStage={stageOne}
            onUnstage={unstageOne}
            onDiscard={(entry) => setDiscarding([entry])}
            onAll={stageEverything}
            allLabel={text("全部暂存", "Stage all")}
            footer={
              unstaged.length > 1 ? (
                <button class="git-danger-link" disabled={busy} onClick={() => setDiscarding(unstaged)}>
                  {text("丢弃全部更改", "Discard all changes")}
                </button>
              ) : undefined
            }
          />

          <section class="git-section">
            <header class="git-section-head">
              <strong>{text("历史", "History")}</strong>
              <span class="git-section-count">{history.length}</span>
            </header>
            {!history.length && (
              <p class="git-section-empty">{text("还没有提交", "No commits yet")}</p>
            )}
            <div class="git-history">
              {history.map((commit) => (
                <button
                  key={commit.hash}
                  class={commitHash === commit.hash ? "active" : ""}
                  title={`${commit.hash}\n${commit.author}`}
                  onClick={() => setSelection({ kind: "commit", hash: commit.hash })}
                >
                  <span class="git-history-subject">{commitSubject(commit.subject)}</span>
                  <span class="git-history-meta">
                    <code>{commit.short}</code>
                    <span>{commit.author}</span>
                    <span>{relativeTime(commit.at, now, zh)}</span>
                  </span>
                </button>
              ))}
            </div>
          </section>

          {nothingToShow && (
            <div class="workbench-empty">
              {loading
                ? text("正在读取变更…", "Loading changes…")
                : notRepository
                  ? text(
                      "当前目录尚未初始化 Git，暂无版本变更",
                      "This directory is not a Git repository. No version changes to show.",
                    )
                  : text("工作区没有文件变更", "No changed files")}
            </div>
          )}
        </div>

        <div class="workbench-git-diff">
          {selection?.kind === "file" && selectedRow ? (
            <GitDiff
              session={session}
              change={selectedRow}
              version={version + refresh}
              staged={selection.staged}
              onStaged={(value) =>
                setSelection({ kind: "file", path: selectedRow.path, staged: value })
              }
            >
              <button
                aria-label={
                  maximized
                    ? text("恢复差异布局", "Restore diff layout")
                    : text("最大化差异", "Maximize diff")
                }
                title={text("最大化 / 恢复（Esc）", "Maximize / restore (Esc)")}
                aria-pressed={maximized}
                onClick={onMaximize}
              >
                {maximized ? "⤡" : "⤢"}
              </button>
              {!isDeleted(selectedRow) && (
                <button onClick={() => onOpen(selectedRow.path)}>{text("打开文件", "Open file")}</button>
              )}
            </GitDiff>
          ) : selection?.kind === "commit" ? (
            <CommitDetail zh={zh} detail={detail} error={detailError} onOpen={onOpen} />
          ) : (
            <div class="workbench-empty git-diff-placeholder">
              <strong>{text("选择变更查看差异", "Select a change to see its diff")}</strong>
              <p>
                {text(
                  "点击左侧文件查看逐行差异；点击历史提交查看该次提交的全部改动。",
                  "Pick a file on the left for a line-by-line diff, or a commit in History to see everything it changed.",
                )}
              </p>
            </div>
          )}
        </div>
      </div>

      {discarding && (
        <ConfirmDialog
          title={text("丢弃这些更改？", "Discard these changes?")}
          body={
            discarding.length === 1
              ? text(
                  `「${discarding[0].path}」的本地修改将无法恢复。`,
                  `Local edits to “${discarding[0].path}” cannot be recovered.`,
                )
              : text(
                  `${discarding.length} 个文件的本地修改将无法恢复。`,
                  `Local edits to ${discarding.length} files cannot be recovered.`,
                )
          }
          confirmLabel={text("丢弃", "Discard")}
          cancelLabel={text("取消", "Cancel")}
          onConfirm={async () => {
            await discardGitPaths(session, selectionsOf(discarding));
            setDiscarding(null);
            setSelection(null);
            setNotice(text("已丢弃更改", "Changes discarded"));
            setRefresh((value) => value + 1);
          }}
          onClose={() => setDiscarding(null)}
        />
      )}
    </div>
  );
}

/** One source-control section: staged or unstaged rows, grouped by folder. */
function ChangeSection({
  title,
  entries,
  staged,
  busy,
  zh,
  selected,
  onSelect,
  onOpen,
  onStage,
  onUnstage,
  onDiscard,
  onAll,
  allLabel,
  footer,
}: {
  title: string;
  entries: GitEntry[];
  staged: boolean;
  busy: boolean;
  zh: boolean;
  selected: Selection | null;
  onSelect: (entry: GitEntry) => void;
  onOpen: (path: string) => void;
  onStage: (entry: GitEntry) => void;
  onUnstage: (entry: GitEntry) => void;
  onDiscard: (entry: GitEntry) => void;
  onAll: () => void;
  allLabel: string;
  footer?: ComponentChildren;
}) {
  const { additions, deletions } = sumStats(entries);
  return (
    <section class="git-section">
      <header class="git-section-head">
        <strong>{title}</strong>
        <span class="git-section-count">{entries.length}</span>
        {entries.length > 0 && (
          <>
            <span class="git-section-stats">
              <span class="git-add">+{additions}</span>
              <span class="git-del">−{deletions}</span>
            </span>
            <button class="git-section-action" disabled={busy} onClick={onAll}>
              {allLabel}
            </button>
          </>
        )}
      </header>
      {!entries.length && (
        <p class="git-section-empty">
          {staged
            ? zh
              ? "暂存区是空的"
              : "Nothing staged"
            : zh
              ? "没有未暂存的更改"
              : "No unstaged changes"}
        </p>
      )}
      {groupByDirectory(entries).map((group) => (
        <div class="git-group" key={group.id || "root"}>
          <div class="git-group-head">
            <code>{group.label === "." ? "/" : `${group.label}/`}</code>
            <span>{group.entries.length}</span>
          </div>
          {group.entries.map((entry) => {
            const active =
              selected?.kind === "file" &&
              selected.path === entry.path &&
              selected.staged === staged;
            return (
              <div class={`git-row${active ? " active" : ""}`} key={`${staged ? "s" : "w"}:${entry.path}`}>
                <button class="git-row-main" title={entry.path} onClick={() => onSelect(entry)}>
                  <span class={`git-badge git-kind-${entry.kind}`} aria-hidden="true">
                    {statusLetter(entry.kind)}
                  </span>
                  <span class="git-file-name">{entry.name}</span>
                  {entry.dir && <span class="git-file-dir">{entry.dir}</span>}
                  {(entry.additions > 0 || entry.deletions > 0) && (
                    <span class="git-line-stats">
                      {entry.additions > 0 && <span class="git-add">+{entry.additions}</span>}
                      {entry.deletions > 0 && <span class="git-del">−{entry.deletions}</span>}
                    </span>
                  )}
                </button>
                <span class="git-row-actions">
                  {staged ? (
                    <button
                      disabled={busy}
                      title={zh ? "取消暂存" : "Unstage"}
                      aria-label={zh ? `取消暂存 ${entry.name}` : `Unstage ${entry.name}`}
                      onClick={() => onUnstage(entry)}
                    >
                      −
                    </button>
                  ) : (
                    <button
                      disabled={busy}
                      title={zh ? "暂存" : "Stage"}
                      aria-label={zh ? `暂存 ${entry.name}` : `Stage ${entry.name}`}
                      onClick={() => onStage(entry)}
                    >
                      ＋
                    </button>
                  )}
                  {!staged && entry.kind !== "deleted" && (
                    <button
                      disabled={busy}
                      title={zh ? "丢弃更改" : "Discard changes"}
                      aria-label={zh ? `丢弃 ${entry.name} 的更改` : `Discard changes in ${entry.name}`}
                      onClick={() => onDiscard(entry)}
                    >
                      ⟲
                    </button>
                  )}
                  {entry.kind !== "deleted" && (
                    <button
                      disabled={busy}
                      title={zh ? "打开文件" : "Open file"}
                      aria-label={zh ? `打开 ${entry.name}` : `Open ${entry.name}`}
                      onClick={() => onOpen(entry.path)}
                    >
                      ↗
                    </button>
                  )}
                </span>
              </div>
            );
          })}
        </div>
      ))}
      {footer}
    </section>
  );
}

/** What one commit changed: its files and the whole diff, read-only. */
function CommitDetail({
  zh,
  detail,
  error,
  onOpen,
}: {
  zh: boolean;
  detail: GitCommitDetail | null;
  error: string;
  onOpen: (path: string) => void;
}) {
  if (error) {
    return (
      <div role="alert" class="workbench-error">
        <p>{error}</p>
      </div>
    );
  }
  if (!detail) {
    return (
      <div class="workbench-empty">{zh ? "正在读取提交…" : "Loading commit…"}</div>
    );
  }
  return (
    <div class="git-commit-detail">
      <div class="git-commit-detail-head">
        <strong>{zh ? "提交内容" : "Commit changes"}</strong>
        <span>{zh ? `${detail.files.length} 个文件` : `${detail.files.length} files`}</span>
      </div>
      <div class="git-commit-files">
        {detail.files.map((file) => {
          const letter = statusLetter(statusKind(file.index));
          return (
            <button key={file.path} title={file.path} onClick={() => onOpen(file.path)}>
              <span class={`git-badge git-kind-${statusKind(file.index)}`} aria-hidden="true">
                {letter}
              </span>
              <span class="git-file-name">{file.path}</span>
              {file.stats && (
                <span class="git-line-stats">
                  {file.stats.additions > 0 && <span class="git-add">+{file.stats.additions}</span>}
                  {file.stats.deletions > 0 && <span class="git-del">−{file.stats.deletions}</span>}
                </span>
              )}
            </button>
          );
        })}
        {!detail.files.length && (
          <p class="git-section-empty">{zh ? "这次提交没有文件改动" : "This commit changed no files"}</p>
        )}
      </div>
      {detail.diff ? (
        <DiffLines text={detail.diff} label={zh ? "该提交的差异" : "Diff of this commit"} />
      ) : (
        <div class="workbench-empty">{zh ? "该提交没有文本差异" : "This commit has no text diff"}</div>
      )}
    </div>
  );
}

function DiffLines({ text, label }: { text: string; label: string }) {
  return (
    <pre class="workbench-diff" aria-label={label}>
      {text
        ? text.split("\n").map((line, index) => (
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
        : " "}
    </pre>
  );
}
