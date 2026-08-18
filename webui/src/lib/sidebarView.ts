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

/** A grouped workspace view owns one complete project bucket; a flat view is cross-project. */
export function sidebarProjectScope(
  mode: SidebarViewMode,
  projectHash?: string,
): string | null {
  return mode === 'workspace' && projectHash ? projectHash : null;
}
