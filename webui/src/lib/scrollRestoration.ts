/** Chat-local scroll intent. Browser layout/visibility changes are not user scrolls. */
export interface ChatViewport { scrollTop: number; scrollHeight: number; clientHeight: number }
export function createScrollRestoration(options: {
  viewport: () => ChatViewport | null;
  /** Size local reply space before reading scroll geometry. */
  prepareLayout?: () => void;
  /** Allow a new oversized prompt to open at its beginning once. */
  followingTop?: (bottom: number) => number;
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
  let lastTop: number | null = null;
  function stopFrame() { if (frame !== null) options.cancel(frame); frame = null; }
  function capture(): Position {
    return { following: options.following(), top: options.viewport()?.scrollTop ?? 0, session: options.session() };
  }
  function apply(position: Position): boolean {
    options.prepareLayout?.();
    const el = options.viewport();
    if (!el || el.clientHeight <= 0) return false;
    const bottom = Math.max(0, el.scrollHeight - el.clientHeight);
    el.scrollTop = position.following ? (options.followingTop?.(bottom) ?? bottom)
      : Math.min(position.top, Math.max(0, el.scrollHeight - el.clientHeight));
    lastTop = el.scrollTop;
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
      else options.prepareLayout?.();
    },
    onScroll(): boolean | null {
      const el = options.viewport();
      if (disposed || suspended || restoring || !el || el.clientHeight <= 0) return null;
      const movedUp = lastTop !== null && el.scrollTop < lastTop - 0.5;
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= 2;
      // Layout growth is not user intent; reading ends only at the actual bottom.
      const following = atBottom ? true : movedUp || lastTop === null ? false : options.following();
      lastTop = el.scrollTop;
      return following;
    },
    interact() {
      // A real gesture takes priority over any pending browser-resume correction.
      restoring = null; saved = null; stopFrame();
      lastTop = options.viewport()?.scrollTop ?? null;
    },
    dispose() { disposed = true; stopFrame(); },
  };
}
