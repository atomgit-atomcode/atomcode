import { lazy, Suspense } from "preact/compat";
import { useEffect, useRef, useState } from "preact/hooks";
import { ApiCallError, workbenchPreview, workbenchRequest } from "../api";
import { useSettings } from "../settings";
import "./Workbench.css";
import { Markdown } from "./Markdown";
import { WorkbenchEditor } from "./WorkbenchEditor";
import { WorkbenchGit } from "./WorkbenchGit";
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
  tab: "files" | "terminal" | "git";
  drafts: Record<string, WorkbenchDraft>;
  editing: string[];
  positions: Record<string, number>;
  width: number;
};
const empty = (): Saved => ({
  path: "",
  files: [],
  selected: "",
  terminals: [],
  tab: "files",
  drafts: {},
  editing: [],
  positions: {},
  width: 420,
});
const memory = new WorkbenchCache<Saved>();
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
function FilePreview({
  session,
  path,
  draftText,
  position,
  onPosition,
}: {
  session: string;
  path: string;
  draftText?: string;
  position: number;
  onPosition: (value: number) => void;
}) {
  const view = useRef<HTMLDivElement>(null);
  const [text, setText] = useState("");
  const [image, setImage] = useState("");
  const [error, setError] = useState("");
  useEffect(() => {
    let cancelled = false;
    let url = "";
    setText("");
    setImage("");
    setError("");
    if (draftText !== undefined) {
      setText(draftText);
      return;
    }
    workbenchPreview(session, path)
      .then(async (blob) => {
        if (cancelled) return;
        if (blob.type.startsWith("image/")) {
          url = URL.createObjectURL(blob);
          setImage(url);
        } else {
          const content = await blob.text();
          if (!cancelled) setText(content);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [session, path, draftText]);
  useEffect(() => {
    if (view.current) view.current.scrollTop = position;
  }, [text, image]);
  return (
    <div
      class="workbench-preview"
      ref={view}
      onScroll={(event) => onPosition(event.currentTarget.scrollTop)}
    >
      {error ? (
        <p role="alert" class="workbench-error">
          {error}
        </p>
      ) : image ? (
        <img src={image} alt={path} />
      ) : /\.md$/i.test(path) ? (
        <div class="workbench-markdown">
          <Markdown content={text} />
        </div>
      ) : (
        <pre tabIndex={0}>{text}</pre>
      )}
    </div>
  );
}
export function Workbench({
  session,
  cwd,
  onClose,
  requestedFile,
  onFileOpened,
}: {
  session: string;
  cwd: string;
  onClose: () => void;
  requestedFile?: { session: string; path: string; seq: number } | null;
  onFileOpened?: () => void;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [saved, rawSetSaved] = useState<Saved>(() => load(session));
  const current = useRef(saved);
  const dragCleanup = useRef<(() => void) | null>(null);
  function setSaved(update: Saved | ((old: Saved) => Saved)) {
    const next =
      typeof update === "function"
        ? update(memory.get(session) || current.current)
        : update;
    current.current = next;
    const subscribed = memory.hasSubscribers(session);
    memory.set(session, next);
    if (alive.current && !subscribed) rawSetSaved(next);
    if (!alive.current) {
      try {
        sessionStorage.setItem("workbench:" + session, JSON.stringify(next));
      } catch {
        /* beforeunload warns while drafts are dirty */
      }
    }
  }
  const [entries, setEntries] = useState<Entry[]>([]);
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
  useEffect(() => {
    if (!requestedFile || requestedFile.session !== session) return;
    const root = cwd.replace(/\\/g, "/").replace(/\/$/, "");
    const candidate = requestedFile.path;
    const path = candidate.startsWith(root + "/") ? candidate.slice(root.length + 1) : candidate;
    openFile(path);
    onFileOpened?.();
  }, [requestedFile, session, cwd]);

  useEffect(
    () =>
      memory.subscribe(session, (state) => {
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
    setError("");
    setEntries([]);
    workbenchRequest<Entry[]>(
      "files?" + new URLSearchParams({ session_id: session, path: saved.path }),
    )
      .then((data) => {
        if (!cancelled) setEntries(data);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [session, saved.path, version]);
  const update = (patch: Partial<Saved>) =>
    setSaved((old) => ({ ...old, ...patch }));
  async function createTerminal() {
    if (busy) return;
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
  function openFile(path: string) {
    setSaved((old) => ({
      ...old,
      tab: "files",
      selected: path,
      files: old.files.includes(path) ? old.files : [...old.files, path],
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
        selected: old.selected === file ? "" : old.selected,
      };
    });
  }
  function closePanel() {
    if (
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
  return (
    <aside
      class="workbench"
      style={{ width: saved.width + "px" }}
      aria-label={zh ? "工作区面板" : "Workspace panel"}
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
          <strong>{zh ? "工作区" : "Workspace"}</strong>
          <small title={cwd}>{cwd}</small>
        </div>
        <button
          onClick={closePanel}
          aria-label={zh ? "关闭工作区面板" : "Close workspace panel"}
        >
          ×
        </button>
      </header>
      <nav aria-label={zh ? "工作区视图" : "Workspace views"}>
        <button
          class={saved.tab === "files" ? "active" : ""}
          onClick={() => update({ tab: "files" })}
        >
          {zh ? "文件" : "Files"}
        </button>
        <button
          class={saved.tab === "terminal" ? "active" : ""}
          onClick={() => update({ tab: "terminal" })}
        >
          {zh ? "终端" : "Terminal"}
        </button>
        <button
          class={saved.tab === "git" ? "active" : ""}
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
      {error && (
        <p role="alert" class="workbench-error">
          {error}
        </p>
      )}
      {saved.tab === "files" ? (
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
              {!/\.(png|jpe?g|gif|webp)$/i.test(saved.selected) && (
                <div class="workbench-file-mode">
                  <button
                    class={
                      !saved.editing.includes(saved.selected) ? "active" : ""
                    }
                    onClick={() =>
                      update({
                        editing: saved.editing.filter(
                          (f) => f !== saved.selected,
                        ),
                      })
                    }
                  >
                    {zh ? "预览" : "Preview"}
                  </button>
                  <button
                    class={
                      saved.editing.includes(saved.selected) ? "active" : ""
                    }
                    onClick={() =>
                      update({ editing: [...saved.editing, saved.selected] })
                    }
                  >
                    {zh ? "编辑" : "Edit"}
                  </button>
                  <code title={saved.selected}>{saved.selected}</code>
                </div>
              )}
              {saved.editing.includes(saved.selected) ? (
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
                <FilePreview
                  key={saved.selected}
                  session={session}
                  path={saved.selected}
                  draftText={saved.drafts[saved.selected]?.text}
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
                {zh
                  ? "选择文件以预览或编辑。支持文本、代码和常见图片，最大 2 MiB。"
                  : "Select a file to preview or edit. Text, code and common images up to 2 MiB."}
              </p>
            </div>
          )}
        </>
      ) : saved.tab === "git" ? (
        <WorkbenchGit session={session} version={version} onOpen={openFile} />
      ) : (
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
