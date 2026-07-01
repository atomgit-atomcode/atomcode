import hljs from 'highlight.js';

const ANSI_PATTERN = /\x1b\[[0-9;]*m/g;
const ANSI_FG_CLASSES: Record<number, string> = {
  30: 'ansi-fg-black',
  31: 'ansi-fg-red',
  32: 'ansi-fg-green',
  33: 'ansi-fg-yellow',
  34: 'ansi-fg-blue',
  35: 'ansi-fg-magenta',
  36: 'ansi-fg-cyan',
  37: 'ansi-fg-white',
  90: 'ansi-fg-bright-black',
  91: 'ansi-fg-bright-red',
  92: 'ansi-fg-bright-green',
  93: 'ansi-fg-bright-yellow',
  94: 'ansi-fg-bright-blue',
  95: 'ansi-fg-bright-magenta',
  96: 'ansi-fg-bright-cyan',
  97: 'ansi-fg-bright-white',
};

type DiffCodeLineType = 'add' | 'del' | 'ctx' | 'hunk' | 'meta';

interface DiffCodeLine {
  type: DiffCodeLineType;
  marker: '+' | '-' | ' ';
  content: string;
  empty?: boolean;
}

export function escapeHtml(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function ansiCodeToHtml(code: string): string {
  let output = '';
  let lastIndex = 0;
  let currentClass: string | null = null;
  ANSI_PATTERN.lastIndex = 0;

  const closeSpan = () => {
    if (currentClass) {
      output += '</span>';
      currentClass = null;
    }
  };

  for (const match of code.matchAll(ANSI_PATTERN)) {
    output += escapeHtml(code.slice(lastIndex, match.index));
    lastIndex = (match.index ?? 0) + match[0].length;

    const rawCodes = match[0].slice(2, -1);
    const codes = rawCodes.length > 0 ? rawCodes.split(';').map((value) => Number(value) || 0) : [0];
    for (const sgr of codes) {
      if (sgr === 0 || sgr === 39) {
        closeSpan();
      } else if (ANSI_FG_CLASSES[sgr]) {
        closeSpan();
        currentClass = ANSI_FG_CLASSES[sgr];
        output += `<span class="${currentClass}">`;
      }
    }
  }

  output += escapeHtml(code.slice(lastIndex));
  closeSpan();
  return output;
}

function languageFromInfo(infostring?: string): string {
  const lang = (infostring ?? '').trim().split(/\s+/)[0] ?? '';
  return lang.toLowerCase();
}

function highlightLanguage(language: string): string {
  if (!language || language === 'diff' || language === 'patch') return '';
  return hljs.getLanguage(language) ? language : '';
}

function isDiffLanguage(language: string): boolean {
  return language === 'diff' || language === 'patch';
}

function stripOneTrailingNewline(text: string): string {
  return text.endsWith('\n') ? text.slice(0, -1) : text;
}

function isDiffSentinelLine(line: string): boolean {
  return /^(diff|patch)$/i.test(line.trim());
}

function diffContentLines(text: string): string[] {
  return stripOneTrailingNewline(text)
    .split('\n')
    .filter((line) => !isDiffSentinelLine(line));
}

function isDiffMetaLine(line: string): boolean {
  return (
    line.startsWith('diff --git ') ||
    line.startsWith('index ') ||
    line.startsWith('new file mode ') ||
    line.startsWith('deleted file mode ') ||
    line.startsWith('old mode ') ||
    line.startsWith('new mode ') ||
    line.startsWith('similarity index ') ||
    line.startsWith('dissimilarity index ') ||
    line.startsWith('rename from ') ||
    line.startsWith('rename to ') ||
    line.startsWith('copy from ') ||
    line.startsWith('copy to ') ||
    line.startsWith('--- ') ||
    line.startsWith('+++ ')
  );
}

function shouldRenderAsDiff(text: string, language: string): boolean {
  if (isDiffLanguage(language)) return true;

  const nonEmptyLines = diffContentLines(text).filter((line) => line.trim().length > 0);
  if (nonEmptyLines.length === 0) return false;

  const hasChangedLine = nonEmptyLines.some((line) => line.startsWith('+') || line.startsWith('-'));
  if (!hasChangedLine) return false;

  return nonEmptyLines.every((line) =>
    line.startsWith('+') ||
    line.startsWith('-') ||
    line.startsWith(' ') ||
    line.startsWith('@@') ||
    isDiffMetaLine(line),
  );
}

function parseDiffCodeLines(text: string): DiffCodeLine[] {
  return diffContentLines(text).map((line) => {
    if (line.length === 0) {
      return { type: 'ctx', marker: ' ', content: '', empty: true };
    }
    if (line.startsWith('@@')) {
      return { type: 'hunk', marker: ' ', content: line };
    }
    if (isDiffMetaLine(line)) {
      return { type: 'meta', marker: ' ', content: line };
    }
    if (line.startsWith('+')) {
      return { type: 'add', marker: '+', content: line.slice(1) };
    }
    if (line.startsWith('-')) {
      return { type: 'del', marker: '-', content: line.slice(1) };
    }
    if (line.startsWith(' ')) {
      return { type: 'ctx', marker: ' ', content: line.slice(1) };
    }
    return { type: 'ctx', marker: ' ', content: line };
  });
}

function highlightCode(code: string, language: string): string {
  const hasAnsi = ANSI_PATTERN.test(code);
  ANSI_PATTERN.lastIndex = 0;
  if (hasAnsi) return ansiCodeToHtml(code);
  if (language) return hljs.highlight(code, { language }).value;
  return escapeHtml(code);
}

function diffCodeToHtml(text: string, language: string): string {
  const syntaxLanguage = highlightLanguage(language);
  return parseDiffCodeLines(text).map((line) => {
    const displayContent = line.content.length > 0 ? line.content : ' ';
    const highlighted = line.type === 'hunk' || line.type === 'meta'
      ? escapeHtml(displayContent)
      : highlightCode(displayContent, syntaxLanguage);

    const lineClasses = [
      'diff-code-line',
      line.empty ? 'diff-code-empty' : '',
      `diff-code-${line.type}`,
    ].filter(Boolean).join(' ');

    return (
      `<span class="${lineClasses}">` +
      `<span class="diff-code-gutter">${escapeHtml(line.marker)}</span>` +
      `<span class="diff-code-content">${highlighted}</span>` +
      `</span>`
    );
  }).join('');
}

export function renderCodeBlockHtml(
  code: string,
  infostring?: string,
  labels: { copy?: string } = {},
): string {
  const text = code ?? '';
  if (!text.trim()) {
    return '';
  }

  const rawLanguage = languageFromInfo(infostring);
  const language = highlightLanguage(rawLanguage);
  const lines = stripOneTrailingNewline(text).split('\n');
  const nonEmptyLines = lines.filter((line) => line.trim().length > 0);
  const isSingleLine = nonEmptyLines.length <= 1;
  const isShortBlock = nonEmptyLines.length <= 3 && text.length < 240;
  const isDiffLike = shouldRenderAsDiff(text, rawLanguage);
  const highlighted = isDiffLike
    ? diffCodeToHtml(text, rawLanguage)
    : language
      ? highlightCode(text, language)
      : isShortBlock
        ? escapeHtml(text)
        : hljs.highlightAuto(text).value;
  const id = `cb-${Math.random().toString(36).slice(2, 8)}`;
  const classes = [
    'code-block-wrapper',
    isSingleLine ? 'is-single-line' : '',
    isShortBlock ? 'is-short-block' : '',
    isDiffLike ? 'is-diff-like' : '',
    language ? 'has-language' : 'no-language',
  ].filter(Boolean).join(' ');
  const codeClass = `hljs${language ? ` language-${language}` : ''}`;

  return (
    `<div class="${classes}" data-code-id="${id}" data-raw-code="${escapeHtml(text)}">` +
    `<pre><code class="${codeClass}">${highlighted}</code></pre>` +
    `<button class="copy-button" data-action="copy" title="${escapeHtml(labels.copy ?? 'Copy')}">` +
    `<svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">` +
    `<path d="M4 4v8h8V4H4zm7 7H5V5h6v6zM2 2v8h1V3h7V2H2z"/>` +
    `</svg></button>` +
    `</div>`
  );
}
