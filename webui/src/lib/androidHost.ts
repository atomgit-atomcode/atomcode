// The Android host bridge.
//
// The webui is the same bundle the desktop serves, so it cannot call into the
// app directly. On Android the page is loaded into a `WebView` that has one
// object injected into it (`AtomCodeHost`), and this module is the only place
// that knows about it.
//
// Everything here is optional: on the desktop there is no bridge at all, so
// `installHelperApk` reports "no host" instead of throwing. The caller turns
// that into a message rather than a crash — the page has to keep working in a
// plain browser.

interface HostBridge {
  openExternal?(url: string): void;
  installHelper?(path: string): void;
  /** JSON array of the provider package ids that are installed right now. */
  installedProviders?(): string;
  startExternalDaemon?(): void;
  stopExternalDaemon?(): void;
  sessionsPrivate?(): boolean;
  setSessionsPrivate?(appPrivate: boolean): void;
}

declare global {
  interface Window {
    AtomCodeHost?: HostBridge;
  }
}

/** The injected host object, or null when the page is not in the Android app. */
function host(): HostBridge | null {
  if (typeof window === 'undefined') return null;
  return window.AtomCodeHost ?? null;
}

/** Whether the page runs inside the Android app (so the bridge is there). */
export function hasAndroidHost(): boolean {
  return host() !== null;
}

/**
 * Ask the app to install the helper APK at `path`.
 *
 * Deliberately returns nothing. The host hands the file to the system
 * installer, which runs in its own UI — the result is not available at call
 * time, and it is not even reliably delivered back as an activity result (the
 * installer on ColorOS never returns one). So this only *dispatches* the
 * request; the caller finds out by asking the daemon for the helper status
 * again, which is the only truthful source.
 *
 * Returns `false` only when there is no host at all (a desktop browser).
 */
export function installHelperApk(path: string): boolean {
  const bridge = host();
  if (!bridge?.installHelper) return false;
  try {
    bridge.installHelper(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Which provider APKs (`com.atomcode.tools`, `com.atomcode.tools.zig`) are installed.
 *
 * `null` means "this page cannot know" — a desktop browser has no bridge, so the
 * panel must fall back to the daemon's `<data>/bin` status instead of claiming that
 * nothing is installed. An empty array means the host answered and nothing is there.
 *
 * Per-provider state has to come from the host: the daemon only sees the links in
 * `<data>/bin`, which cannot say *which* APK a tool arrived from.
 */
export function installedProviders(): string[] | null {
  const bridge = host();
  if (!bridge?.installedProviders) return null;
  try {
    const parsed: unknown = JSON.parse(bridge.installedProviders());
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((value): value is string => typeof value === 'string');
  } catch {
    return [];
  }
}

/**
 * Ask the app to start the bundled CLI as an external daemon and switch to it.
 *
 * Nothing is returned: the daemon takes seconds to come up, and the host reports
 * the outcome through its own UI (a toast, or the normal switch). This is only
 * meaningful on Android — on the desktop there is no bridge and no helper APK to
 * get the CLI from.
 */
export function startExternalDaemon(): boolean {
  const bridge = host();
  if (!bridge?.startExternalDaemon) return false;
  try {
    bridge.startExternalDaemon();
    return true;
  } catch {
    return false;
  }
}

/** Stop the daemon the app started and fall back to the in-app agent. */
export function stopExternalDaemon(): boolean {
  const bridge = host();
  if (!bridge?.stopExternalDaemon) return false;
  try {
    bridge.stopExternalDaemon();
    return true;
  } catch {
    return false;
  }
}

/**
 * Whether the session log is kept app-private, or null on a non-Android host.
 *
 * A single file check on the other side, so this is safe to call while
 * rendering the toggle.
 */
export function sessionsPrivate(): boolean | null {
  const bridge = host();
  if (!bridge?.sessionsPrivate) return null;
  try {
    return bridge.sessionsPrivate() === true;
  } catch {
    return null;
  }
}

/**
 * Move the session log into (or out of) app-private storage.
 *
 * Takes effect on the next process start — the host pins the session root once —
 * so the caller must say so instead of assuming the change is live.
 */
export function setSessionsPrivate(appPrivate: boolean): boolean {
  const bridge = host();
  if (!bridge?.setSessionsPrivate) return false;
  try {
    bridge.setSessionsPrivate(appPrivate);
    return true;
  } catch {
    return false;
  }
}
