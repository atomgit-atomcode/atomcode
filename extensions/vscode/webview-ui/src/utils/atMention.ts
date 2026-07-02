export interface AtMentionRange {
  start: number;
  end: number;
  token: string;
}

export interface SplitAtToken {
  scopeDir: string;
  filter: string;
}

export interface AtMentionSelection {
  text: string;
  cursor: number;
  keepOpen: boolean;
  query: string;
}

export interface ScrollContainerMetrics {
  scrollTop: number;
  clientHeight: number;
}

export interface ScrollDescendantMetrics {
  offsetTop: number;
  offsetHeight: number;
}

export function detectAtMentionRange(text: string, cursor: number): AtMentionRange | null {
  const prefix = text.slice(0, cursor);
  const start = prefix.lastIndexOf('@');
  if (start < 0) return null;
  if (start > 0 && !/\s/.test(prefix[start - 1])) return null;

  const tokenToCursor = prefix.slice(start + 1);
  if (/\s/.test(tokenToCursor)) return null;

  const afterAt = text.slice(start + 1);
  const whitespace = afterAt.search(/\s/);
  const tokenLength = whitespace >= 0 ? whitespace : afterAt.length;
  const end = start + 1 + tokenLength;
  return { start, end, token: text.slice(start + 1, end) };
}

export function splitAtToken(token: string): SplitAtToken {
  const slash = token.lastIndexOf('/');
  if (slash < 0) return { scopeDir: '', filter: token };
  return { scopeDir: token.slice(0, slash + 1), filter: token.slice(slash + 1) };
}

export function replaceAtMention(
  text: string,
  range: AtMentionRange,
  selectedPath: string,
): { text: string; cursor: number } {
  const replacement = `@${selectedPath}`;
  const next = text.slice(0, range.start) + replacement + text.slice(range.end);
  return { text: next, cursor: range.start + replacement.length };
}

export function applyAtMentionSelection(
  text: string,
  range: AtMentionRange,
  selectedPath: string,
  isDir: boolean,
): AtMentionSelection {
  const next = replaceAtMention(text, range, selectedPath);
  return {
    ...next,
    keepOpen: isDir,
    query: isDir ? selectedPath : '',
  };
}

export function ensureActiveDescendantVisible(
  container: ScrollContainerMetrics,
  active: ScrollDescendantMetrics,
) {
  const viewTop = container.scrollTop;
  const viewBottom = viewTop + container.clientHeight;
  const activeTop = active.offsetTop;
  const activeBottom = activeTop + active.offsetHeight;

  if (activeTop < viewTop) {
    container.scrollTop = activeTop;
  } else if (activeBottom > viewBottom) {
    container.scrollTop = activeBottom - container.clientHeight;
  }
}
