export type SidebarViewMode = 'workspace' | 'flat';

const SIDEBAR_VIEW_STORAGE_KEY = 'atomcode.sidebar.view.v1';

interface SidebarViewStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function loadSidebarViewMode(storage?: SidebarViewStorage | null): SidebarViewMode {
  try {
    const saved = storage?.getItem(SIDEBAR_VIEW_STORAGE_KEY);
    return saved === 'flat' ? 'flat' : 'workspace';
  } catch {
    return 'workspace';
  }
}

export function saveSidebarViewMode(
  mode: SidebarViewMode,
  storage?: SidebarViewStorage | null,
): void {
  try {
    storage?.setItem(SIDEBAR_VIEW_STORAGE_KEY, mode);
  } catch {
    // Storage can be unavailable in privacy modes. The in-memory choice still works.
  }
}

/** Return requested expanded project buckets only when workspace grouping is active. */
export function sidebarProjectScopes(
  mode: SidebarViewMode,
  projectHashes: readonly string[],
): string[] {
  return mode === 'workspace' ? [...new Set(projectHashes.filter(Boolean))] : [];
}
