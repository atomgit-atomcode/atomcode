import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { ApiCallError, workbenchPreview, workbenchRequest } from "../api";
import { useSettings, useT } from "../settings";
import { Markdown } from "./Markdown";

/** One `git status --porcelain` row as the daemon reports it. */
export type Change = {
  path: string;
  original?: string;
  index: string;
  worktree: string;
};

/** The repository's current changes. A workspace without Git rejects with an
 *  error `isNotRepository` recognizes, so callers can tell it from a failure. */
export async function readChanges(session: string): Promise<Change[]> {
  return workbenchRequest<Change[]>(
    "git?" + new URLSearchParams({ session_id: session }),
  );
}
export function isNotRepository(error: unknown): boolean {
  return error instanceof ApiCallError && error.code === "workspace_not_git_repository";
}
/** Index side exists only for a staged, tracked change. */
export function hasIndexChange(change: Change): boolean {
  return change.index !== " " && change.index !== "?";
}
export function isDeleted(change: Change): boolean {
  return change.worktree === "D" || change.index === "D";
}

const IMAGE = /^image\/(png|jpeg|gif|webp)$/i;
type Loaded = { url: string; image: boolean; text: string };

/** Read-only file content for one session: text/code, sanitized Markdown with
 *  links routed to the owning session, supported images, and a download of the
 *  exact session-validated preview blob. A draft is shown instead of the disk
 *  copy (and is not downloadable: it has not been saved). */
export function FileContent({
  session,
  path,
  draftText,
  position = 0,
  onPosition,
}: {
  session: string;
  path: string;
  draftText?: string;
  position?: number;
  onPosition?: (value: number) => void;
}) {
  const t = useT();
  const { lang } = useSettings();
  const zh = lang === "zh";
  const view = useRef<HTMLDivElement>(null);
  const [loaded, setLoaded] = useState<Loaded>();
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    setLoaded(undefined);
    setError("");
    if (draftText !== undefined) {
      setLoaded({ url: "", image: false, text: draftText });
      return;
    }
    const controller = new AbortController();
    let url = "";
    workbenchPreview(session, path, controller.signal)
      .then(async (blob) => {
        const image = IMAGE.test(blob.type);
        const text = image ? "" : await blob.text();
        if (controller.signal.aborted) return;
        url = URL.createObjectURL(blob);
        setLoaded({ url, image, text });
      })
      .catch((e) => {
        if (!controller.signal.aborted) setError(String(e));
      });
    return () => {
      controller.abort();
      if (url) URL.revokeObjectURL(url);
    };
  }, [session, path, draftText, attempt]);
  useEffect(() => {
    if (loaded && view.current) view.current.scrollTop = position;
  }, [loaded]);
  const filename = path.split(/[\\/]/).pop() || "download";
  return (
    <div
      class="workbench-preview"
      ref={view}
      // The loading placeholder is short, so the browser clamps scrollTop to 0
      // and fires a scroll; only loaded content may record the reading position.
      onScroll={(event) => {
        if (loaded) onPosition?.(event.currentTarget.scrollTop);
      }}
    >
      {loaded?.url && !error && (
        <div class="workbench-preview-actions">
          <a href={loaded.url} download={filename}>{t("filePreview.download")}</a>
        </div>
      )}
      {error ? (
        <div role="alert" class="workbench-error">
          <strong>{t("filePreview.failed")}</strong>
          <p>{error}</p>
          <button onClick={() => setAttempt((value) => value + 1)}>
            {zh ? "重试" : "Retry"}
          </button>
        </div>
      ) : !loaded ? (
        <p role="status" class="workbench-status">{t("filePreview.loading")}</p>
      ) : loaded.image ? (
        <img src={loaded.url} alt={filename}
          onError={() => setError(t("filePreview.invalidImage"))} />
      ) : /\.(md|markdown)$/i.test(path) ? (
        <div class="workbench-markdown">
          <Markdown content={loaded.text} sessionId={session} documentPath={path} />
        </div>
      ) : (
        <pre tabIndex={0}>{loaded.text}</pre>
      )}
    </div>
  );
}

/** The current Git diff of one changed file: working tree against the index,
 *  or the index against HEAD. It is the repository's state now, never a
 *  snapshot of an earlier turn. `children` adds caller actions to the bar. */
export function GitDiff({
  session,
  change,
  version,
  staged,
  onStaged,
  children,
}: {
  session: string;
  change: Change;
  version: number;
  staged: boolean;
  onStaged: (staged: boolean) => void;
  children?: ComponentChildren;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [diff, setDiff] = useState<string>();
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const useIndex = staged && hasIndexChange(change);
  useEffect(() => {
    let cancelled = false;
    setError("");
    setDiff(undefined);
    workbenchRequest<{ diff: string }>(
      "diff?" +
        new URLSearchParams({
          session_id: session,
          path: change.path,
          staged: String(useIndex),
          untracked: String(change.index === "?"),
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
  }, [session, change.path, change.index, change.worktree, useIndex, version, attempt]);
  return (
    <>
      <div class="workbench-diff-actions">
        <button class={!useIndex ? "active" : ""} aria-pressed={!useIndex}
          title={zh ? "工作区与暂存区的差异" : "Working tree compared with the index"}
          onClick={() => onStaged(false)}>
          {zh ? "工作区" : "Working tree"}
        </button>
        {hasIndexChange(change) && (
          <button class={useIndex ? "active" : ""} aria-pressed={useIndex}
            title={zh ? "暂存区与 HEAD 的差异" : "Index compared with HEAD"}
            onClick={() => onStaged(true)}>
            {zh ? "暂存区" : "Staged"}
          </button>
        )}
        <span title={change.path}>{change.path}</span>
        {children}
      </div>
      {error ? (
        <div role="alert" class="workbench-error">
          <p>{error}</p>
          <button onClick={() => setAttempt((value) => value + 1)}>
            {zh ? "重试" : "Retry"}
          </button>
        </div>
      ) : (
        <pre class="workbench-diff" aria-label={zh ? "当前 Git 差异" : "Current Git diff"}
          aria-busy={diff === undefined} tabIndex={0}>
          {diff === undefined
            ? zh ? "正在读取差异…" : "Loading diff…"
            : diff
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
      )}
    </>
  );
}

/** The current Git diff for a file opened in the file viewer: finds the file's
 *  change first so "not a repository", "unchanged" and failures read apart. */
export function FileGitDiff({
  session,
  path,
  version,
}: {
  session: string;
  path: string;
  version: number;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [state, setState] = useState<
    | { kind: "loading" }
    | { kind: "repository-absent" }
    | { kind: "failed"; error: string }
    | { kind: "ready"; change: Change | null }
  >({ kind: "loading" });
  const [staged, setStaged] = useState<boolean>();
  const [attempt, setAttempt] = useState(0);
  const loadedPath = useRef("");
  useEffect(() => {
    let cancelled = false;
    // Background refreshes keep the current diff mounted; a new file starts over.
    if (loadedPath.current !== session + "\n" + path) {
      loadedPath.current = session + "\n" + path;
      setState({ kind: "loading" });
      setStaged(undefined);
    }
    readChanges(session)
      .then((rows) => {
        if (cancelled) return;
        const change = rows.find((row) => row.path === path) ?? null;
        setState((old) =>
          old.kind === "ready" && old.change && change &&
          old.change.index === change.index && old.change.worktree === change.worktree
            ? old
            : { kind: "ready", change },
        );
      })
      .catch((e) => {
        if (cancelled) return;
        setState(isNotRepository(e) ? { kind: "repository-absent" } : { kind: "failed", error: String(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [session, path, version, attempt]);
  const caption = (
    <div class="workbench-git-caption">
      {zh
        ? "当前 Git 差异 · 仓库此刻的状态，不是历史快照"
        : "Current Git diff · the repository as it is now, not a historical snapshot"}
    </div>
  );
  if (state.kind === "loading")
    return <>{caption}<p role="status" class="workbench-status">{zh ? "正在读取 Git 状态…" : "Loading Git status…"}</p></>;
  if (state.kind === "repository-absent")
    return <>{caption}<div class="workbench-empty" role="status">
      {zh ? "当前目录不是 Git 仓库，没有可比较的版本" : "This directory is not a Git repository. There is no version to compare with."}
    </div></>;
  if (state.kind === "failed")
    return <>{caption}<div role="alert" class="workbench-error">
      <p>{state.error}</p>
      <button onClick={() => setAttempt((value) => value + 1)}>{zh ? "重试" : "Retry"}</button>
    </div></>;
  if (!state.change)
    return <>{caption}<div class="workbench-empty" role="status">
      {zh ? "此文件当前没有 Git 变更" : "This file has no current Git changes"}
    </div></>;
  const change = state.change;
  return (
    <>
      {caption}
      <GitDiff session={session} change={change} version={version}
        staged={staged ?? change.worktree === " "} onStaged={setStaged} />
    </>
  );
}
