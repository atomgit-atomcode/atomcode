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

export function prepareMarkdownForRender(source: string, streaming: boolean): string {
  const protectedSource = protectInlineCodeFenceLines(source);
  return streaming ? repairStreamingMarkdown(protectedSource) : protectedSource;
}
