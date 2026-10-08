// Pieces of the webui's sign-in flow that do not need a component.

/** `{name}` placeholders filled from `values`; an unknown one becomes empty. */
export function fill(text: string, values: Record<string, string>): string {
  return text.replace(/\{(\w+)\}/g, (_, key: string) => values[key] ?? '');
}

/**
 * Open the sign-in page and say whether that worked.
 *
 * Not with the `noopener` feature: with it `window.open` returns `null` whether
 * or not the tab opened, so a blocked pop-up was indistinguishable from a
 * successful one — and in an installed-app window the tab opens in a browser
 * window the person is not looking at. The new tab's handle to this page is
 * cut by hand instead, which is what `noopener` was for.
 */
export function openSignIn(
  url: string,
  open: (url: string, target: string) => { opener: unknown } | null = (u, t) => window.open(u, t),
): boolean {
  const opened = open(url, '_blank');
  if (!opened) return false;
  try {
    opened.opener = null;
  } catch {
    /* cross-origin already: nothing to cut */
  }
  return true;
}
