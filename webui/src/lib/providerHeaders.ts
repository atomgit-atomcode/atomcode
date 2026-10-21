export interface HeaderRow { name: string; value: string; stored?: boolean }
const token = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const transport = new Set(['host', 'content-length', 'transfer-encoding', 'connection', 'trailer', 'upgrade', 'proxy-authorization', 'proxy-authenticate']);
export function headerProblem(rows: HeaderRow[]): boolean {
  const names = new Set<string>();
  return rows.some((row) => {
    const name = row.name.toLowerCase();
    if (!row.name && !row.value && !row.stored) return false;
    if (!token.test(name) || transport.has(name) || names.has(name) || /[\x00-\x08\x0a-\x1f\x7f]/.test(row.value)) return true;
    names.add(name);
    return false;
  });
}
export function headerBody(rows: HeaderRow[]): Record<string, string | null> {
  // Null preserves a stored value without downloading it into the browser.
  return Object.fromEntries(rows.filter((r) => r.name || r.value || r.stored).map((r) => [r.name.toLowerCase(), r.stored && r.value === '' ? null : r.value]));
}
