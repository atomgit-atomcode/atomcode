/** Supported browsers all decode WOFF2. Keep every face, removing only fallback formats. */
export function woff2FontSources(css: string): string {
  return css.replace(/\bsrc\s*:\s*([^;{}]+)/g, (declaration, sources: string) => {
    const woff2 = sources.match(/url\([^)]*\.woff2["']?\)\s*format\(["']woff2["']\)/);
    if (!woff2) return declaration;
    return `src:${woff2[0]}`;
  });
}
