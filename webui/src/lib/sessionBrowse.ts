/** Navigation never transfers ownership of an executing conversation. */
export function sessionSelectionDisposition(bound: string | null, selected: string, executing: boolean, browsing: boolean): 'return' | 'browse' | 'activate' {
  if (bound === selected) return 'return';
  return executing || browsing ? 'browse' : 'activate';
}
