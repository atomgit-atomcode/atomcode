interface FenceState {
  marker: '`' | '~';
  length: number;
}

const INLINE_FENCE_PROTECTOR = '\u200b';

function stripLineBreak(line: string): string {
  return line.replace(/[\r\n]+$/, '');
}

function fenceOpen(line: string): FenceState | null {
  const raw = stripLineBreak(line);
  let index = 0;
  let indent = 0;
  while (index < raw.length && (raw[index] === ' ' || raw[index] === '\t')) {
    indent += 1;
    index += 1;
  }
  if (indent > 3) return null;

  const marker = raw[index];
  if (marker !== '`' && marker !== '~') return null;

  let markerEnd = index;
  while (markerEnd < raw.length && raw[markerEnd] === marker) {
    markerEnd += 1;
  }
  const length = markerEnd - index;
  if (length < 3) return null;

  const info = raw.slice(markerEnd).trim();
  if (marker === '`' && info.includes('`')) return null;

  return { marker, length };
}

function fenceClose(line: string, state: FenceState): boolean {
  const raw = stripLineBreak(line);
  let index = 0;
  let indent = 0;
  while (index < raw.length && (raw[index] === ' ' || raw[index] === '\t')) {
    indent += 1;
    index += 1;
  }
  if (indent > 3) return false;

  let markerEnd = index;
  while (markerEnd < raw.length && raw[markerEnd] === state.marker) {
    markerEnd += 1;
  }

  return markerEnd - index >= state.length && raw.slice(markerEnd).trim() === '';
}

function nextInlineCodeDelimiter(line: string, delimiter: number | null): number | null {
  let index = 0;

  while (index < line.length) {
    if (line[index] !== '`') {
      index += 1;
      continue;
    }

    let end = index;
    while (end < line.length && line[end] === '`') {
      end += 1;
    }

    const length = end - index;
    if (delimiter === null) {
      if (length < 3) {
        delimiter = length;
      }
    } else if (length === delimiter) {
      delimiter = null;
    }

    index = end;
  }

  return delimiter;
}

export function protectInlineCodeFenceLines(source: string): string {
  const text = String(source ?? '');
  let openFence: FenceState | null = null;
  let inlineDelimiter: number | null = null;

  return text.split('\n').map((line) => {
    let output = line;

    if (openFence) {
      if (fenceClose(line, openFence)) {
        openFence = null;
      }
      return output;
    }

    const openingFence = fenceOpen(line);
    if (inlineDelimiter === null && openingFence) {
      openFence = openingFence;
      return output;
    }

    if (inlineDelimiter !== null && openingFence) {
      output = `${INLINE_FENCE_PROTECTOR}${line}`;
    }

    inlineDelimiter = nextInlineCodeDelimiter(line, inlineDelimiter);
    return output;
  }).join('\n');
}

export function repairStreamingMarkdown(source: string): string {
  const text = String(source ?? '');
  let open: FenceState | null = null;

  for (const line of text.split('\n')) {
    if (!open) {
      open = fenceOpen(line);
    } else if (fenceClose(line, open)) {
      open = null;
    }
  }

  if (!open) return text;
  const fence = open.marker.repeat(open.length);
  return `${text}${text.endsWith('\n') ? '' : '\n'}${fence}\n`;
}

function hasUnescapedPipe(line: string): boolean {
  let escaped = false;
  for (const ch of line) {
    if (ch === '\\') {
      escaped = !escaped;
      continue;
    }
    if (ch === '|' && !escaped) return true;
    escaped = false;
  }
  return false;
}

function isTableDelimiterLine(line: string): boolean {
  const cells = line.trim().replace(/^\|/, '').replace(/\|$/, '').split('|');
  return cells.length >= 2 && cells.every((cell) => /^\s*:?-+:?\s*$/.test(cell));
}

type HtmlBlockState = { kind: 'untilBlank' } | { kind: 'untilPattern'; pattern: RegExp };

const HTML_BLOCK_TAGS = [
  'address', 'article', 'aside', 'base', 'basefont', 'blockquote', 'body', 'caption',
  'center', 'col', 'colgroup', 'dd', 'details', 'dialog', 'dir', 'div', 'dl', 'dt',
  'fieldset', 'figcaption', 'figure', 'footer', 'form', 'frame', 'frameset', 'h1',
  'h2', 'h3', 'h4', 'h5', 'h6', 'head', 'header', 'hr', 'html', 'iframe', 'legend',
  'li', 'link', 'main', 'menu', 'menuitem', 'nav', 'noframes', 'ol', 'optgroup',
  'option', 'p', 'param', 'search', 'section', 'summary', 'table', 'tbody', 'td',
  'tfoot', 'th', 'thead', 'title', 'tr', 'track', 'ul',
].join('|');

function htmlBlockOpen(line: string): HtmlBlockState | null {
  const trimmed = line.trimStart();
  if (!/^ {0,3}</.test(line)) return null;
  if (/^<(script|pre|style|textarea)(?=[\s>])/i.test(trimmed)) {
    const tag = trimmed.match(/^<([A-Za-z][\w-]*)/i)?.[1] ?? '';
    return { kind: 'untilPattern', pattern: new RegExp(`</${tag}\\s*>`, 'i') };
  }
  if (/^<!--/.test(trimmed)) return { kind: 'untilPattern', pattern: /-->/ };
  if (/^<\?/.test(trimmed)) return { kind: 'untilPattern', pattern: /\?>/ };
  if (/^<![A-Z]/.test(trimmed)) return { kind: 'untilPattern', pattern: />/ };
  if (/^<!\[CDATA\[/.test(trimmed)) return { kind: 'untilPattern', pattern: /\]\]>/ };
  if (new RegExp(`^</?(${HTML_BLOCK_TAGS})(?=\\s|>|/>)`, 'i').test(trimmed)) {
    return { kind: 'untilBlank' };
  }
  return null;
}

function htmlBlockClose(line: string, state: HtmlBlockState): boolean {
  if (state.kind === 'untilBlank') return line.trim() === '';
  return state.pattern.test(line);
}

function isMarkdownBlockStart(line: string): boolean {
  const trimmed = line.trim();
  if (!trimmed) return true;
  return (
    /^#{1,6}\s/.test(trimmed) ||
    /^>/.test(trimmed) ||
    /^([-+*]|\d+[.)])\s/.test(trimmed) ||
    /^(```|~~~)/.test(trimmed) ||
    /^([-*_]\s*){3,}$/.test(trimmed) ||
    /^<\/?[A-Za-z][\w-]*(\s|>|\/>)/.test(trimmed)
  );
}

function separateTableFromFollowingParagraph(source: string): string {
  const lines = String(source ?? '').split('\n');
  const output: string[] = [];
  let inTable = false;
  let openFence: FenceState | null = null;
  let openHtmlBlock: HtmlBlockState | null = null;

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const nextLine = lines[i + 1] ?? '';

    if (openHtmlBlock) {
      output.push(line);
      if (htmlBlockClose(line, openHtmlBlock)) {
        openHtmlBlock = null;
      }
      continue;
    }

    if (openFence) {
      output.push(line);
      if (fenceClose(line, openFence)) {
        openFence = null;
      }
      continue;
    }

    const openingFence = fenceOpen(line);
    if (openingFence) {
      if (inTable) {
        output.push('');
        inTable = false;
      }
      output.push(line);
      openFence = openingFence;
      continue;
    }

    const openingHtmlBlock = htmlBlockOpen(line);
    if (openingHtmlBlock) {
      output.push(line);
      openHtmlBlock = htmlBlockClose(line, openingHtmlBlock) ? null : openingHtmlBlock;
      inTable = false;
      continue;
    }

    if (inTable && line.trim() && !hasUnescapedPipe(line) && !isMarkdownBlockStart(line)) {
      output.push('');
      inTable = false;
    }

    output.push(line);

    if (hasUnescapedPipe(line) && isTableDelimiterLine(nextLine)) {
      inTable = true;
    } else if (inTable && (!line.trim() || isMarkdownBlockStart(line))) {
      inTable = false;
    }
  }

  return output.join('\n');
}

export function prepareMarkdownForRender(source: string, streaming: boolean): string {
  const tableSafeSource = separateTableFromFollowingParagraph(source);
  const protectedSource = protectInlineCodeFenceLines(tableSafeSource);
  return streaming ? repairStreamingMarkdown(protectedSource) : protectedSource;
}
