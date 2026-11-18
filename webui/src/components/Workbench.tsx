import { lazy, Suspense } from "preact/compat";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { ApiCallError, workbenchRequest } from "../api";
import { useSettings } from "../settings";
import "./Workbench.css";
import { WorkbenchEditor } from "./WorkbenchEditor";
import { WorkbenchGit } from "./WorkbenchGit";
import { FileContent, FileGitDiff } from "./WorkbenchViewer";
import { WorkbenchCache } from "../lib/workbenchCache";
import { draftDirty, type WorkbenchDraft } from "../lib/workbenchDraft";
const TerminalView = lazy(() =>
  import("./WorkbenchTerminal").then((m) => ({ default: m.WorkbenchTerminal })),
);
type Entry = { name: string; directory: boolean };
type Saved = {
  path: string;
  files: string[];
  selected: string;
  terminals: string[];
  tab: "home" | "files" | "terminal" | "git";
  drafts: Record<string, WorkbenchDraft>;
  editing: string[];
  /** Open files showing their current Git diff instead of content. */
  diffing: string[];
  positions: Record<string, number>;
  width: number;
};
const empty = (): Saved => ({
  path: "",
  files: [],
  selected: "",
  terminals: [],
  tab: "home",
  drafts: {},
  editing: [],
  diffing: [],
  positions: {},
  width: 420,
});
const memory = new WorkbenchCache<Saved>();
/** Presentation state of read-only (historical session) panels. Kept apart
 *  from `memory` so browsing a session never sees or touches its drafts, and
 *  never persisted: it holds no drafts or terminals worth restoring. */
const readOnlyMemory = new WorkbenchCache<Saved>();
export function persistWorkbenchState() {
  for (const [session, state] of memory) {
    try {
      sessionStorage.setItem("workbench:" + session, JSON.stringify(state));
    } catch {
      /* dirty drafts still trigger the unload warning */
    }
  }
}
export function hasUnsavedWorkbenchDrafts(): boolean {
  if (
    [...memory.values()].some((state) =>
      Object.values(state.drafts).some(draftDirty),
    )
  )
    return true;
  try {
    for (let i = 0; i < sessionStorage.length; i++) {
      const key = sessionStorage.key(i);
      if (!key?.startsWith("workbench:") || memory.has(key.slice(10))) continue;
      const state = JSON.parse(sessionStorage.getItem(key) || "null");
      if (
        state?.drafts &&
        Object.values(state.drafts).some((draft) =>
          draftDirty(draft as WorkbenchDraft),
        )
      )
        return true;
    }
  } catch {
    /* memory state above remains authoritative */
  }
  return false;
}

function load(session: string): Saved {
  if (memory.has(session)) return memory.get(session)!;
  try {
    const data = JSON.parse(
      sessionStorage.getItem("workbench:" + session) || "null",
    );
    if (data && Array.isArray(data.files) && Array.isArray(data.terminals)) {
      const loaded = { ...empty(), ...data };
      memory.set(session, loaded);
      return loaded;
    }
  } catch {
    /* tab storage unavailable */
  }
  return empty();
}
/** Last chat git/file request seq each panel owner has applied. Seqs come
 *  from one monotonic counter, so a single watermark covers both kinds. */
const appliedRequests = new Map<string, number>();
/** Claims a request once; remounts (close/reopen, session switch) must not
 *  replay it, since the user may have moved on from that tab or file. */
function claimRequest(owner: string, request: { sessionId: string; seq: number } | undefined, session: string) {
  if (!request || request.sessionId !== session) return false;
  if ((appliedRequests.get(owner) ?? 0) >= request.seq) return false;
  appliedRequests.set(owner, request.seq);
  return true;
}
export function Workbench({
  session,
  cwd,
  onClose,
  gitRequest,
  fileRequest,
  readOnly = false,
  title,
}: {
  session: string;
  cwd: string;
  onClose: () => void;
  gitRequest?: { sessionId: string; path: string; seq: number };
  fileRequest?: { sessionId: string; path: string; seq: number };
  /** A historical session being browsed: files and current Git diffs only —
   *  no editor, save, terminal or other mutating call, and separate state. */
  readOnly?: boolean;
  /** Session name shown on a read-only panel. */
  title?: string;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const cache = readOnly ? readOnlyMemory : memory;
  const panel = useRef<HTMLElement>(null);
  const [fullscreen, setFullscreen] = useState(false);
  const [contentMaximized, setContentMaximized] = useState(false);
  useLayoutEffect(() => {
    if (!fullscreen && !contentMaximized) return;
    const escape = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      event.preventDefault();
      if (contentMaximized) setContentMaximized(false);
      else setFullscreen(false);
    };
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [fullscreen, contentMaximized]);
  const [saved, rawSetSaved] = useState<Saved>(() =>
    readOnly ? readOnlyMemory.get(session) ?? empty() : load(session),
  );
  useEffect(() => setContentMaximized(false), [session, saved.tab]);
  useEffect(() => {
    if (saved.tab === "files" && !saved.files.includes(saved.selected)) setContentMaximized(false);
  }, [saved.tab, saved.selected, saved.files]);
  const current = useRef(saved);
  const dragCleanup = useRef<(() => void) | null>(null);
  function setSaved(update: Saved | ((old: Saved) => Saved)) {
    const next =
      typeof update === "function"
        ? update(cache.get(session) || current.current)
        : update;
    current.current = next;
    const subscribed = cache.hasSubscribers(session);
    cache.set(session, next);
    if (alive.current && !subscribed) rawSetSaved(next);
    if (!alive.current && !readOnly) {
      try {
        sessionStorage.setItem("workbench:" + session, JSON.stringify(next));
      } catch {
        /* beforeunload warns while drafts are dirty */
      }
    }
  }
  const [entries, setEntries] = useState<Entry[]>([]);
  const directory = useRef<{ session: string; path: string; loaded: boolean } | null>(null);
  const [filesState, setFilesState] = useState<"loading" | "ready" | "failed">("loading");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [version, setVersion] = useState(0);
  const alive = useRef(true);
  useEffect(() => {
    const refresh = () => {
      if (document.visibilityState === "visible") setVersion(v => v + 1);
    };
    const timer = setInterval(refresh, 10000);
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", refresh);
    return () => {
      clearInterval(timer);
      window.removeEventListener("focus", refresh);
      document.removeEventListener("visibilitychange", refresh);
    };
  }, [session]);

  useEffect(
    () =>
      cache.subscribe(session, (state) => {
        current.current = state;
        rawSetSaved(state);
      }),
    [session],
  );
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      dragCleanup.current?.();
      if (readOnly) return;
      try {
        sessionStorage.setItem(
          "workbench:" + session,
          JSON.stringify(current.current),
        );
      } catch {
        /* in-memory fallback */
      }
    };
  }, []);
  useEffect(() => {
    if (readOnly) return;
    const timer = setTimeout(() => {
      try {
        sessionStorage.setItem("workbench:" + session, JSON.stringify(saved));
      } catch {
        setError(
          zh
            ? "浏览器无法保存草稿；请保持页面打开并及时保存文件。"
            : "Draft storage is unavailable; keep this page open and save your files.",
        );
      }
    }, 400);
    return () => clearTimeout(timer);
  }, [saved, session]);
  useEffect(() => {
    let cancelled = false;
    if (directory.current?.session !== session || directory.current.path !== saved.path) {
      directory.current = { session, path: saved.path, loaded: false };
    }
    const scope = directory.current;
    // Polling and focus refreshes must not unmount the current rows: doing so
    // flashes the tree and resets its scroll/focus even when nothing changed.
    // A new directory still clears the old rows while its first load is pending.
    if (!scope.loaded) {
      setError("");
      setEntries([]);
      setFilesState("loading");
    }
    workbenchRequest<Entry[]>(
      "files?" + new URLSearchParams({ session_id: session, path: saved.path }),
    )
      .then((data) => {
        if (!cancelled) {
          scope.loaded = true;
          setError("");
          setEntries(old => old.length === data.length && old.every((entry, i) =>
            entry.name === data[i].name && entry.directory === data[i].directory,
          ) ? old : data);
          setFilesState("ready");
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setError(String(e));
          setFilesState("failed");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [session, saved.path, version]);
  const update = (patch: Partial<Saved>) =>
    setSaved((old) => ({ ...old, ...patch }));
  // Read-only panels claim under their own key: the same session may also
  // have had a mutable panel, and neither should consume the other's request.
  const requestOwner = (readOnly ? "read-only:" : "") + session;
  useEffect(() => {
    if (!claimRequest(requestOwner, gitRequest, session)) return;
    update({ tab: "git" });
    setVersion(value => value + 1);
    panel.current?.focus({ preventScroll: true });
  }, [session, gitRequest?.seq]);
  useEffect(() => {
    if (!fileRequest || !claimRequest(requestOwner, fileRequest, session)) return;
    openFile(fileRequest.path, "content");
    panel.current?.focus({ preventScroll: true });
  }, [session, fileRequest?.seq]);
  async function createTerminal() {
    if (busy || readOnly) return;
    setBusy(true);
    setError("");
    try {
      const result = await workbenchRequest<{ id: string }>("terminals", {
        session_id: session,
      });
      if (!alive.current) {
        await workbenchRequest(
          "terminals/" +
            result.id +
            "?" +
            new URLSearchParams({ session_id: session }),
          undefined,
          "DELETE",
        );
        return;
      }
      setSaved((old) => ({
        ...old,
        terminals: [...old.terminals, result.id],
        selected: result.id,
        tab: "terminal",
      }));
    } catch (e) {
      if (alive.current) setError(String(e));
    } finally {
      if (alive.current) setBusy(false);
    }
  }
  async function closeTerminal(id: string) {
    if (readOnly) return false;
    try {
      await workbenchRequest(
        "terminals/" + id + "?" + new URLSearchParams({ session_id: session }),
        undefined,
        "DELETE",
      );
    } catch (e) {
      if (!(e instanceof ApiCallError && e.status === 404)) {
        setError(String(e));
        return false;
      }
    }
    setSaved((old) => ({
      ...old,
      terminals: old.terminals.filter((t) => t !== id),
      selected: old.selected === id ? "" : old.selected,
    }));
    return true;
  }
  function openFile(path: string, view?: "content") {
    setSaved((old) => ({
      ...old,
      tab: "files",
      selected: path,
      files: old.files.includes(path) ? old.files : [...old.files, path],
      // A chat link asks for the content even if this file last showed a diff
      // or the editor; its draft stays and other files keep their modes.
      editing: view === "content" ? old.editing.filter((f) => f !== path) : old.editing,
      diffing: view === "content" ? old.diffing.filter((f) => f !== path) : old.diffing,
    }));
  }
  function setFileMode(path: string, mode: "content" | "edit" | "diff") {
    if (readOnly && mode === "edit") return;
    setSaved((old) => ({
      ...old,
      editing: mode === "edit"
        ? old.editing.includes(path) ? old.editing : [...old.editing, path]
        : old.editing.filter((f) => f !== path),
      diffing: mode === "diff"
        ? old.diffing.includes(path) ? old.diffing : [...old.diffing, path]
        : old.diffing.filter((f) => f !== path),
    }));
  }
  function closeFile(file: string) {
    if (
      draftDirty(current.current.drafts[file]) &&
      !confirm(
        zh
          ? "关闭文件会丢弃它的未保存草稿，确定继续？"
          : "Close this file and discard its unsaved draft?",
      )
    )
      return;
    setSaved((old) => {
      const drafts = { ...old.drafts };
      delete drafts[file];
      return {
        ...old,
        drafts,
        files: old.files.filter((f) => f !== file),
        editing: old.editing.filter((f) => f !== file),
        diffing: old.diffing.filter((f) => f !== file),
        selected: old.selected === file ? "" : old.selected,
      };
    });
  }
  function closePanel() {
    if (
      !readOnly &&
      Object.values(current.current.drafts).some(draftDirty) &&
      !confirm(
        zh
          ? "有未保存的修改，草稿会保留在当前浏览器标签页。仍要关闭面板？"
          : "Unsaved changes remain. Keep drafts in this browser tab and close the panel?",
      )
    )
      return;
    onClose();
  }
  function resize(event: PointerEvent) {
    event.preventDefault();
    dragCleanup.current?.();
    const start = event.clientX;
    const width = current.current.width;
    const move = (e: PointerEvent) =>
      update({
        width: Math.max(
          300,
          Math.min(900, window.innerWidth * 0.6, width + start - e.clientX),
        ),
      });
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      dragCleanup.current = null;
    };
    dragCleanup.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }
  // Escape closes the panel where it covers the chat (mobile full screen) or
  // holds nothing to lose (read-only); a desktop editing panel stays open.
  // Inputs and the terminal keep Escape for themselves.
  function escapeToClose(event: KeyboardEvent) {
    if (event.key !== "Escape" || event.defaultPrevented || fullscreen || contentMaximized) return;
    const target = event.target as HTMLElement | null;
    if (target?.closest("textarea, input, select, [contenteditable], .workbench-terminal")) return;
    if (!readOnly && !window.matchMedia("(max-width: 850px)").matches) return;
    event.preventDefault();
    closePanel();
  }
  const terminalsAllowed = !readOnly;
  const tab = !terminalsAllowed && saved.tab === "terminal" ? "home" : saved.tab;
  const fileMode = !saved.files.includes(saved.selected)
    ? "content"
    : !readOnly && saved.editing.includes(saved.selected)
      ? "edit"
      : saved.diffing.includes(saved.selected) ? "diff" : "content";
  return (
    <aside
      ref={panel}
      tabIndex={-1}
      class={"workbench" + ((fullscreen || contentMaximized) ? " workbench-fullscreen" : "") + (contentMaximized ? " workbench-content-maximized" : "") + (readOnly ? " workbench-read-only" : "")}
      style={{ width: saved.width + "px" }}
      aria-label={readOnly
        ? (zh ? "只读工作区面板：" : "Read-only workspace panel: ") + (title || session)
        : zh ? "工作区面板" : "Workspace panel"}
      onKeyDown={escapeToClose}
    >
      <div
        class="workbench-resizer"
        role="separator"
        aria-orientation="vertical"
        aria-label={zh ? "调整工作区宽度" : "Resize workspace"}
        tabIndex={0}
        onPointerDown={resize}
        onKeyDown={(event) => {
          if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
            event.preventDefault();
            update({
              width: Math.max(
                300,
                Math.min(
                  900,
                  window.innerWidth * 0.6,
                  saved.width + (event.key === "ArrowLeft" ? 20 : -20),
                ),
              ),
            });
          }
        }}
      />
      <header>
        <div>
          <strong>
            {readOnly ? (zh ? "工作区 · 只读" : "Workspace · read-only") : zh ? "工作区" : "Workspace"}
          </strong>
          {readOnly && title && <small class="workbench-session-name" title={title}>{title}</small>}
          <small title={cwd}>{cwd}</small>
        </div>
        <div class="workbench-header-actions">
        <button
          hidden={contentMaximized}
          onClick={() => setFullscreen(value => !value)}
          aria-label={zh ? (fullscreen ? "退出工作区全屏" : "工作区全屏") : (fullscreen ? "Exit workspace fullscreen" : "Fullscreen workspace")}
          title={zh ? (fullscreen ? "退出全屏（Esc）" : "全屏") : (fullscreen ? "Exit fullscreen (Esc)" : "Fullscreen")}
          aria-pressed={fullscreen}
        >{fullscreen ? "⤡" : "⤢"}</button>
        <button
          onClick={closePanel}
          aria-label={zh ? "关闭工作区面板" : "Close workspace panel"}
        >
          ×
        </button>
        </div>
      </header>
      <nav aria-label={zh ? "工作区视图" : "Workspace views"}>
        <button class={tab === "home" ? "active" : ""} onClick={() => update({ tab: "home" })}>
          {zh ? "开始" : "Start"}
        </button>
        <button
          class={tab === "files" ? "active" : ""}
          onClick={() => update({ tab: "files" })}
        >
          {zh ? "文件" : "Files"}
        </button>
        {terminalsAllowed && <button
          class={tab === "terminal" ? "active" : ""}
          onClick={() => update({ tab: "terminal" })}
        >
          {zh ? "终端" : "Terminal"}
        </button>}
        <button
          class={tab === "git" ? "active" : ""}
          onClick={() => update({ tab: "git" })}
        >
          Git
        </button>
        <span />
        <button
          title={zh ? "刷新文件" : "Refresh files"}
          onClick={() => setVersion((v) => v + 1)}
        >
          ↻
        </button>
      </nav>
      {readOnly && (
        <p role="note" class="workbench-read-only-note">
          {zh
            ? "正在浏览历史会话：可查看文件与当前 Git 差异，不能编辑、保存或打开终端。内容是磁盘此刻的状态，不是当时的快照。"
            : "Browsing a past session: view files and current Git diffs only — no editing, saving or terminals. Content is the disk as it is now, not a snapshot from then."}
        </p>
      )}
      {error && (
        <p role="alert" class="workbench-error">
          {error}
        </p>
      )}
      {tab === "home" ? (
        <div class="workbench-home">
          <div class="workbench-home-heading">
            <span aria-hidden="true" class="workbench-home-symbol">⌘</span>
            <h2>{zh ? "项目工作区" : "Project workspace"}</h2>
            <p>{readOnly
              ? zh ? "浏览文件或检查当前代码变更" : "Browse files or review current changes"
              : zh ? "浏览文件、运行命令或检查代码变更" : "Browse files, run commands, or review changes"}</p>
          </div>
          <div class="workbench-home-options">
            <button onClick={() => update({ tab: "files" })}>
              <span class="workbench-home-icon files" aria-hidden="true">▤</span>
              <span><strong>{zh ? "文件" : "Files"}</strong><small>{readOnly
                ? zh ? "浏览与预览项目文件" : "Browse and preview project files"
                : zh ? "浏览、预览与编辑项目文件" : "Browse, preview, and edit project files"}</small></span>
              <span aria-hidden="true">→</span>
            </button>
            {terminalsAllowed && <button disabled={busy} onClick={createTerminal}>
              <span class="workbench-home-icon terminal" aria-hidden="true">›_</span>
              <span><strong>{zh ? "新建终端" : "New terminal"}</strong><small>{zh ? "在当前项目中打开终端" : "Open a terminal in this project"}</small></span>
              <span aria-hidden="true">＋</span>
            </button>}
            <button onClick={() => update({ tab: "git" })}>
              <span class="workbench-home-icon git" aria-hidden="true">⑂</span>
              <span><strong>Git</strong><small>{zh ? "查看工作区变更与差异" : "Inspect working tree changes and diffs"}</small></span>
              <span aria-hidden="true">→</span>
            </button>
          </div>
        </div>
      ) : tab === "files" ? (
        <>
          <div class="workbench-path">
            <button
              disabled={!saved.path}
              onClick={() =>
                update({ path: saved.path.split("/").slice(0, -1).join("/") })
              }
            >
              ↑
            </button>
            <code>{saved.path || "/"}</code>
          </div>
          <div
            class="workbench-tree"
            aria-label={zh ? "文件列表" : "File list"}
          >
            {filesState === "loading" && <p role="status">{zh ? "正在加载目录…" : "Loading directory…"}</p>}
            {filesState === "failed" && <p role="status">
              {zh ? "目录加载失败。" : "Directory could not be loaded."}
              <button onClick={() => setVersion(v => v + 1)}>{zh ? "重试" : "Retry"}</button>
            </p>}
            {filesState === "ready" && entries.length === 0 && <p role="status">{zh ? "空目录" : "Empty directory"}</p>}
            {entries.map((entry) => (
              <button
                key={entry.name}
                onClick={() => {
                  const path = [saved.path, entry.name]
                    .filter(Boolean)
                    .join("/");
                  if (entry.directory) update({ path });
                  else openFile(path);
                }}
              >
                <span>{entry.directory ? "▸" : "·"}</span>
                {entry.name}
                {entry.directory && <span class="workbench-folder">/</span>}
              </button>
            ))}
          </div>
          <div class="workbench-tabs">
            {saved.files.map((file) => (
              <div key={file} class={saved.selected === file ? "active" : ""}>
                <button onClick={() => update({ selected: file })} title={file}>
                  {file.split("/").pop()}
                  {draftDirty(saved.drafts[file]) ? " ●" : ""}
                </button>
                <button
                  aria-label={(zh ? "关闭 " : "Close ") + file}
                  onClick={() => closeFile(file)}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
          {saved.files.includes(saved.selected) ? (
            <>
              <div class="workbench-file-mode" role="group" aria-label={zh ? "文件视图" : "File view"}>
                  <button
                    class={fileMode === "content" ? "active" : ""}
                    aria-pressed={fileMode === "content"}
                    onClick={() => setFileMode(saved.selected, "content")}
                  >
                    {zh ? "内容" : "Content"}
                  </button>
                  {!readOnly && <button
                    disabled={/\.(png|jpe?g|gif|webp)$/i.test(saved.selected)}
                    class={fileMode === "edit" ? "active" : ""}
                    aria-pressed={fileMode === "edit"}
                    onClick={() => setFileMode(saved.selected, "edit")}
                  >
                    {zh ? "编辑" : "Edit"}
                  </button>}
                  <button
                    class={fileMode === "diff" ? "active" : ""}
                    aria-pressed={fileMode === "diff"}
                    title={zh ? "仓库此刻的差异，不是历史快照" : "The repository's diff now, not a historical snapshot"}
                    onClick={() => setFileMode(saved.selected, "diff")}
                  >
                    {zh ? "当前 Git 差异" : "Current Git diff"}
                  </button>
                  <code title={saved.selected}>{saved.selected}</code>
                  <button
                    aria-label={zh ? (contentMaximized ? "恢复文件布局" : "最大化文件") : (contentMaximized ? "Restore file layout" : "Maximize file")}
                    title={zh ? "最大化 / 恢复（Esc）" : "Maximize / restore (Esc)"}
                    aria-pressed={contentMaximized}
                    onClick={() => setContentMaximized(v => !v)}
                  >{contentMaximized ? "⤡" : "⤢"}</button>
              </div>
              {fileMode === "diff" ? (
                <FileGitDiff key={saved.selected} session={session}
                  path={saved.selected} version={version} />
              ) : fileMode === "edit" ? (
                <WorkbenchEditor
                  key={saved.selected}
                  session={session}
                  path={saved.selected}
                  draft={saved.drafts[saved.selected]}
                  onDraft={(draft) => {
                    const path = saved.selected;
                    setSaved((old) => ({
                      ...old,
                      drafts: { ...old.drafts, [path]: draft },
                    }));
                  }}
                  onSaved={(revision, original) => {
                    const path = saved.selected;
                    setSaved((old) => {
                      const draft = old.drafts[path];
                      return !draft
                        ? old
                        : {
                            ...old,
                            drafts: {
                              ...old.drafts,
                              [path]: { ...draft, revision, original },
                            },
                          };
                    });
                    if (alive.current) setVersion((v) => v + 1);
                  }}
                />
              ) : (
                <FileContent
                  key={saved.selected}
                  session={session}
                  path={saved.selected}
                  draftText={readOnly ? undefined : saved.drafts[saved.selected]?.text}
                  position={saved.positions[saved.selected] || 0}
                  onPosition={(value) =>
                    setSaved((old) => ({
                      ...old,
                      positions: { ...old.positions, [saved.selected]: value },
                    }))
                  }
                />
              )}
            </>
          ) : (
            <div class="workbench-empty">
              <strong>{zh ? "浏览项目文件" : "Explore project files"}</strong>
              <p>
                {readOnly
                  ? zh
                    ? "选择文件以查看。支持文本、代码和常见图片，最大 2 MiB。"
                    : "Select a file to view. Text, code and common images up to 2 MiB."
                  : zh
                    ? "选择文件以预览或编辑。支持文本、代码和常见图片，最大 2 MiB。"
                    : "Select a file to preview or edit. Text, code and common images up to 2 MiB."}
              </p>
            </div>
          )}
        </>
      ) : tab === "git" ? (
        <WorkbenchGit session={session} version={version} onOpen={(path) => openFile(path, "content")}
          requested={gitRequest?.sessionId === session ? gitRequest : undefined}
          maximized={contentMaximized} onMaximize={() => setContentMaximized(v => !v)}
          onRestore={() => setContentMaximized(false)} />
      ) : !terminalsAllowed ? null : (
        <>
          <div class="workbench-terminal-actions">
            <button disabled={busy} onClick={createTerminal}>
              ＋ {zh ? "新建终端" : "New terminal"}
            </button>
            <small>
              {zh
                ? "在运行 AtomCode 的主机上执行"
                : "Runs on the AtomCode host"}
            </small>
          </div>
          <div class="workbench-tabs">
            {saved.terminals.map((id, index) => (
              <div key={id} class={saved.selected === id ? "active" : ""}>
                <button onClick={() => update({ selected: id })}>
                  {zh ? "终端" : "Terminal"} {index + 1}
                </button>
                <button
                  aria-label={
                    (zh ? "关闭终端 " : "Close terminal ") + (index + 1)
                  }
                  onClick={() => closeTerminal(id)}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
          {saved.terminals.includes(saved.selected) ? (
            <Suspense fallback={<div class="workbench-empty">…</div>}>
              <TerminalView
                key={saved.selected}
                session={session}
                id={saved.selected}
                onRestart={async () => { if (await closeTerminal(saved.selected)) await createTerminal(); }}
              />
            </Suspense>
          ) : (
            <div class="workbench-empty">
              <strong>{zh ? "项目终端" : "Project terminal"}</strong>
              <p>
                {zh
                  ? "新建终端以运行命令。关闭面板不会结束进程；关闭终端页签会结束进程。15 分钟未访问将自动回收。"
                  : "Create a terminal to run commands. Closing this panel keeps processes; closing a terminal ends it. Unattended terminals expire after 15 minutes."}
              </p>
            </div>
          )}
        </>
      )}
    </aside>
  );
}
