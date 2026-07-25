import type { TokenizerAndRendererExtension, Tokens } from 'marked';
import { escapeHtml } from './codeBlockRendering';

export interface VscodeFileTarget {
  path: string;
  line?: number;
  column?: number;
}

export interface OpenFileMessage {
  type: 'openFile';
  path: string;
  startLine?: number;
  startColumn?: number;
}

const BARE_VSCODE_FILE_URI = /^vscode:\/\/file\/[^\s<>"'`]+/i;
const TRAILING_PUNCTUATION = /[.,;!?，。；！？、]$/;

export function parseVscodeFileUri(value: string): VscodeFileTarget | null {
  let uri: URL;
  try {
    uri = new URL(value);
  } catch {
    return null;
  }
  if (uri.protocol.toLowerCase() !== 'vscode:' || uri.hostname.toLowerCase() !== 'file') {
    return null;
  }
  if (uri.username || uri.password || uri.port || uri.search || uri.hash) {
    return null;
  }

  let path: string;
  try {
    path = decodeURIComponent(uri.pathname);
  } catch {
    return null;
  }

  let line: number | undefined;
  let column: number | undefined;
  const position = /:(\d+)(?::(\d+))?$/.exec(path);
  if (position) {
    line = Number(position[1]);
    column = position[2] === undefined ? undefined : Number(position[2]);
    if (!Number.isSafeInteger(line) || line < 1
      || (column !== undefined && (!Number.isSafeInteger(column) || column < 1))) {
      return null;
    }
    path = path.slice(0, position.index);
  }

  // WHATWG URL paths retain a leading slash before a Windows drive letter.
  if (/^\/[a-zA-Z]:[\\/]/.test(path)) {
    path = path.slice(1);
  }
  if (!path) return null;
  return { path, line, column };
}

export function openFileMessageFromVscodeUri(value: string): OpenFileMessage | null {
  const target = parseVscodeFileUri(value);
  if (!target) return null;
  return {
    type: 'openFile',
    path: target.path,
    startLine: target.line,
    startColumn: target.column,
  };
}

function trimBareUri(candidate: string): string {
  let uri = candidate;
  while (TRAILING_PUNCTUATION.test(uri)) {
    uri = uri.slice(0, -1);
  }
  for (const [open, close] of [['(', ')'], ['[', ']']] as const) {
    while (uri.endsWith(close)
      && uri.split(close).length > uri.split(open).length) {
      uri = uri.slice(0, -1);
    }
  }
  return uri;
}

export function renderVscodeFileAnchor(uri: string, text: string): string {
  return `<a href="#" class="vscode-file-link" data-vscode-file-uri="${escapeHtml(uri)}">${text}</a>`;
}

export const vscodeFileLinkExtension: TokenizerAndRendererExtension = {
  name: 'vscodeFileLink',
  level: 'inline',
  start(src: string) {
    const index = src.toLowerCase().indexOf('vscode://file/');
    return index < 0 ? undefined : index;
  },
  tokenizer(src: string) {
    if (this.lexer.state.inLink) return undefined;
    const candidate = BARE_VSCODE_FILE_URI.exec(src)?.[0];
    if (!candidate) return undefined;
    const uri = trimBareUri(candidate);
    if (!parseVscodeFileUri(uri)) return undefined;
    return {
      type: 'vscodeFileLink',
      raw: uri,
      uri,
      text: uri,
    } as Tokens.Generic;
  },
  renderer(token: Tokens.Generic) {
    const uri = String(token.uri ?? '');
    return renderVscodeFileAnchor(uri, escapeHtml(String(token.text ?? uri)));
  },
};
