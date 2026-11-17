/** Resolve local Markdown destinations, never external URLs or browser-only schemes.
 * Paths stay session-relative; the daemon validates canonical paths against its root.
 */
export function localFilePath(href: string, documentPath?: string): string | null {
  let value = href.trim();
  if (!value || /^[#?]/.test(value) || /^[\\/]{2}/.test(value) || /[\x00-\x1f\x7f]/.test(value)) return null;
  if (/^file:/i.test(value)) {
    try {
      const url = new URL(value);
      if (url.hostname && url.hostname !== 'localhost') return null;
      value = decodeURIComponent(url.pathname);
      // file:///C:/... is a drive path on the CLI host, not a POSIX /C: directory.
      if (/^\/[a-z]:\//i.test(value)) value = value.slice(1);
    } catch { return null; }
  } else {
    if (/^[a-z][a-z\d+.-]*:/i.test(value) && !/^[a-z]:[\\/]/i.test(value)) return null;
    try { value = decodeURIComponent(value.split(/[?#]/, 1)[0]); } catch { return null; }
  }
  if (!value || /[\x00-\x1f\x7f]/.test(value) || /^[\\/]{2}/.test(value)) return null;
  // Do not interpret an encoded URL as a filename (or request a network share).
  if (/^[a-z][a-z\d+.-]*:/i.test(value) && !/^[a-z]:[\\/]/i.test(value)) return null;
  if (documentPath && !/^(?:\/|[a-z]:[\\/]|~[\\/])/i.test(value)) {
    const parent = documentPath.slice(0, Math.max(documentPath.lastIndexOf('/'), documentPath.lastIndexOf('\\')) + 1);
    return parent + value;
  }
  return value;
}
