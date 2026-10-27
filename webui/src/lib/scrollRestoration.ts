/** Chat-local scroll intent. Browser layout/visibility changes are not user scrolls. */
export interface ChatViewport { scrollTop: number; scrollHeight: number; clientHeight: number }
export function createScrollRestoration(options: {
  viewport: () => ChatViewport | null;
  session: () => string | null;
  following: () => boolean;
  setFollowing: (following: boolean) => void;
  schedule: (run: () => void) => number;
  cancel: (id: number) => void;
}) {
  type Position = { following: boolean; top: number; session: string | null };
  let saved: Position | null = null;
  let restoring: Position | null = null;
  let suspended = false;
  let disposed = false;
  let frame: number | null = null;
  function stopFrame() { if (frame !== null) options.cancel(frame); frame = null; }
  function capture(): Position {
    return { following: options.following(), top: options.viewport()?.scrollTop ?? 0, session: options.session() };
  }
  function apply(position: Position): boolean {
    const el = options.viewport();
    if (!el || el.clientHeight <= 0) return false;
    el.scrollTop = position.following ? Math.max(0, el.scrollHeight - el.clientHeight)
      : Math.min(position.top, Math.max(0, el.scrollHeight - el.clientHeight));
    return true;
  }
  function finishRestore() {
    stopFrame();
    let passes = 0;
    const settle = () => {
      frame = null;
      if (disposed || suspended || !restoring) return;
      if (restoring.session !== options.session()) { restoring = null; saved = null; return; }
      if (!apply(restoring)) return; // Keep the snapshot until layout becomes visible.
      if (++passes < 2) frame = options.schedule(settle);
      else { restoring = null; saved = null; }
    };
    frame = options.schedule(settle);
  }
  return {
    suspend() {
      if (disposed || suspended) return;
      saved = restoring ?? capture();
      restoring = null;
      suspended = true;
      stopFrame();
    },
    resume() {
      if (disposed) return;
      const position = saved ?? capture();
      suspended = false;
      if (position.session !== options.session()) {
        saved = null; restoring = null; stopFrame();
        if (options.following()) apply(capture());
        return;
      }
      restoring = position;
      options.setFollowing(position.following);
      apply(position);
      finishRestore();
    },
    layout() {
      if (disposed || suspended) return;
      if (restoring) {
        if (restoring.session !== options.session()) { restoring = null; saved = null; return; }
        if (apply(restoring) && frame === null) finishRestore();
      } else if (options.following()) apply(capture());
    },
    onScroll(): boolean | null {
      const el = options.viewport();
      if (disposed || suspended || restoring || !el || el.clientHeight <= 0) return null;
      return el.scrollHeight - el.scrollTop - el.clientHeight <= 80;
    },
    interact() {
      // A real gesture takes priority over any pending browser-resume correction.
      restoring = null; saved = null; stopFrame();
    },
    dispose() { disposed = true; stopFrame(); },
  };
}
