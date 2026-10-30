import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { ApiCallError, workbenchRequest } from "../api";
import {
  draftContent,
  draftDirty,
  draftFromFile,
  type WorkbenchDraft,
} from "../lib/workbenchDraft";
import { useSettings } from "../settings";

export function WorkbenchEditor({
  session,
  path,
  draft,
  onDraft,
  onSaved,
}: {
  session: string;
  path: string;
  draft?: WorkbenchDraft;
  onDraft: (draft: WorkbenchDraft) => void;
  onSaved: (revision: string, original: string) => void;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const [error, setError] = useState("");
  const [conflict, setConflict] = useState(false);
  const [busy, setBusy] = useState(false);
  const [reloading, setReloading] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const alive = useRef(true);
  const latest = useRef(draft);
  latest.current = draft;
  const change = useRef(onDraft);
  change.current = onDraft;
  function publish(next: WorkbenchDraft) {
    latest.current = next;
    change.current(next);
  }
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  useEffect(() => {
    if (latest.current) return;
    let cancelled = false;
    workbenchRequest<{ content: string; revision: string }>(
      "text?" + new URLSearchParams({ session_id: session, path }),
    )
      .then((file) => {
        if (!cancelled) publish(draftFromFile(file.content, file.revision));
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [session, path]);
  const restored = useRef(false);
  useLayoutEffect(() => {
    if (!draft || !area.current || restored.current) return;
    restored.current = true;
    area.current.setSelectionRange(draft.cursor || 0, draft.cursor || 0);
    area.current.scrollTop = draft.scroll || 0;
  }, [draft]);
  async function save() {
    const sent = latest.current;
    if (!sent || !draftDirty(sent) || busy || conflict) return;
    setBusy(true);
    setError("");
    setConflict(false);
    try {
      const result = await workbenchRequest<{ revision: string }>("text", {
        session_id: session,
        path,
        content: draftContent(sent),
        revision: sent.revision,
      });
      // The parent updates its session cache even if this editor unmounted.
      // It merges the baseline into the latest draft, preserving newer typing.
      onSaved(result.revision, sent.text);
    } catch (e) {
      if (alive.current) {
        setError(String(e));
        setConflict(e instanceof ApiCallError && e.status === 409);
      }
    } finally {
      if (alive.current) setBusy(false);
    }
  }
  async function reload() {
    if (busy) return;
    if (
      draftDirty(latest.current) &&
      !confirm(
        zh
          ? "重新载入会丢弃这个文件的未保存草稿，确定继续？"
          : "Reload and discard this file’s unsaved draft?",
      )
    )
      return;
    setBusy(true);
    setReloading(true);
    setError("");
    try {
      const file = await workbenchRequest<{
        content: string;
        revision: string;
      }>("text?" + new URLSearchParams({ session_id: session, path }));
      if (alive.current) {
        publish(draftFromFile(file.content, file.revision));
        setConflict(false);
      }
    } catch (e) {
      if (alive.current) setError(String(e));
    } finally {
      if (alive.current) {
        setBusy(false);
        setReloading(false);
      }
    }
  }
  return (
    <div class="workbench-editor">
      <div class="workbench-editor-actions">
        <span>
          {draftDirty(draft)
            ? zh
              ? "未保存"
              : "Unsaved"
            : zh
              ? "已保存"
              : "Saved"}
        </span>
        <button onClick={reload} disabled={busy}>
          {zh ? "重新载入" : "Reload"}
        </button>
        <button
          class="workbench-save"
          disabled={busy || !draftDirty(draft) || conflict}
          onClick={save}
        >
          {busy ? "…" : zh ? "保存" : "Save"}
        </button>
      </div>
      {error && (
        <p role="alert" class="workbench-error">
          {conflict
            ? zh
              ? "文件已被外部修改，未覆盖。草稿仍保留，请重新载入后再编辑。"
              : "The file changed externally and was not overwritten. Your draft is preserved; reload before editing again."
            : error}
        </p>
      )}
      {draft ? (
        <textarea
          ref={area}
          aria-label={zh ? "文件编辑器" : "File editor"}
          readOnly={reloading}
          spellcheck={false}
          wrap="off"
          value={draft.text}
          onInput={(event) =>
            publish({
              ...latest.current!,
              text: event.currentTarget.value,
              cursor: event.currentTarget.selectionStart,
              scroll: event.currentTarget.scrollTop,
            })
          }
          onSelect={(event) => {
            if (latest.current)
              publish({
                ...latest.current,
                cursor: event.currentTarget.selectionStart,
              });
          }}
          onScroll={(event) => {
            if (latest.current)
              publish({
                ...latest.current,
                scroll: event.currentTarget.scrollTop,
              });
          }}
          onKeyDown={(event) => {
            if (
              (event.ctrlKey || event.metaKey) &&
              event.key.toLowerCase() === "s"
            ) {
              event.preventDefault();
              void save();
            }
            if (event.key === "Tab" && !event.shiftKey && !reloading) {
              event.preventDefault();
              const el = event.currentTarget;
              const start = el.selectionStart;
              const end = el.selectionEnd;
              const next =
                el.value.slice(0, start) + "  " + el.value.slice(end);
              publish({ ...latest.current!, text: next, cursor: start + 2 });
              requestAnimationFrame(() =>
                el.setSelectionRange(start + 2, start + 2),
              );
            }
          }}
        />
      ) : (
        !error && (
          <div class="workbench-empty">
            {zh ? "正在读取文件…" : "Loading file…"}
          </div>
        )
      )}
    </div>
  );
}
