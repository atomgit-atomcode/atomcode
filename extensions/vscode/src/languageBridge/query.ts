/** Version 1 is deliberately read-only: no caller-supplied IDE command is accepted. */
export const operations = ['hover', 'definition', 'references', 'diagnostics', 'document_symbols', 'inspect'] as const;
export type Operation = typeof operations[number];
export type QueryOperation = Exclude<Operation, 'inspect'>;
export interface Query {
  operation: Operation;
  file_path: string;
  workspace_root: string;
  line?: number;
  character?: number;
  symbol?: string;
  occurrence?: number;
  severity?: 'error' | 'warning' | 'all';
  timeout_secs?: number;
}
export interface Position { line: number; character: number }
export interface Document {
  uri: string;
  languageId: string;
  version(): number;
  dirty(): boolean;
  lines(): string[];
}
export interface EditorHost {
  open(filePath: string): Promise<Document>;
  query(operation: QueryOperation, document: Document, position: Position | undefined, severity: string): Promise<unknown[]>;
}
export class QueryError extends Error {
  constructor(readonly status: string, message: string) { super(message); }
}

export function parseQuery(input: unknown): Query {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new QueryError('invalid_request', 'Expected a query object.');
  const q = input as Record<string, unknown>;
  if (!operations.includes(q.operation as Operation) || typeof q.file_path !== 'string' || !q.file_path ||
      typeof q.workspace_root !== 'string' || !q.workspace_root) throw new QueryError('invalid_request', 'Operation, file_path and workspace_root are required.');
  for (const key of ['line', 'character', 'occurrence']) {
    if (q[key] !== undefined && (!Number.isSafeInteger(q[key]) || (q[key] as number) < 1)) throw new QueryError('invalid_request', `${key} must be a positive integer.`);
  }
  if (q.symbol !== undefined && (typeof q.symbol !== 'string' || !q.symbol || q.symbol.length > 1024 || /[\r\n]/u.test(q.symbol))) throw new QueryError('invalid_request', 'symbol must be a nonempty single-line literal (at most 1024 characters).');
  if (q.character !== undefined && q.symbol !== undefined) throw new QueryError('invalid_request', 'Use character or symbol, not both.');
  if (q.occurrence !== undefined && q.symbol === undefined) throw new QueryError('invalid_request', 'occurrence requires symbol.');
  if (q.timeout_secs !== undefined && (typeof q.timeout_secs !== 'number' || !Number.isFinite(q.timeout_secs) || q.timeout_secs < 1 || q.timeout_secs > 120)) throw new QueryError('invalid_request', 'timeout_secs must be between 1 and 120.');
  if (q.severity !== undefined && !['error', 'warning', 'all'].includes(q.severity as string)) throw new QueryError('invalid_request', 'Invalid diagnostic severity.');
  if (!['diagnostics', 'document_symbols'].includes(q.operation as string) &&
      (q.line === undefined || (q.character === undefined && q.symbol === undefined))) throw new QueryError('invalid_request', 'This operation requires line and character or symbol.');
  return q as unknown as Query;
}

/** JavaScript offsets are UTF-16 offsets, exactly like VS Code positions. */
export function resolvePosition(lines: string[], q: Query): Position | undefined {
  if (q.operation === 'diagnostics' || q.operation === 'document_symbols') return undefined;
  const line = (q.line ?? 0) - 1;
  if (line < 0 || line >= lines.length) throw new QueryError('invalid_position', 'Line is outside the editor document.');
  const text = lines[line];
  let character = (q.character ?? 0) - 1;
  if (q.symbol !== undefined) {
    const matches: number[] = [];
    let from = 0;
    while (from <= text.length) {
      const at = text.indexOf(q.symbol, from);
      if (at < 0) break;
      matches.push(at);
      from = at + q.symbol.length;
    }
    if (!matches.length) throw new QueryError('invalid_position', 'Symbol was not found on this line.');
    if (matches.length > 1 && q.occurrence === undefined) throw new QueryError('ambiguous_position', `Symbol occurs ${matches.length} times; specify occurrence (one-based).`);
    character = matches[(q.occurrence ?? 1) - 1];
    if (character === undefined) throw new QueryError('invalid_position', 'Symbol occurrence is outside this line.');
  }
  if (character < 0 || character > text.length) throw new QueryError('invalid_position', 'Character is outside this editor line.');
  if (character > 0 && /[\uD800-\uDBFF]/u.test(text[character - 1]) && /[\uDC00-\uDFFF]/u.test(text[character] ?? '')) throw new QueryError('invalid_position', 'Character splits a UTF-16 surrogate pair.');
  return { line, character };
}

export async function runQuery(host: EditorHost, filePath: string, q: Query) {
  const doc = await host.open(filePath);
  const version = doc.version();
  const position = resolvePosition(doc.lines(), q);
  const requested: QueryOperation[] = q.operation === 'inspect' ? ['hover', 'definition', 'references'] : [q.operation];
  const results = await Promise.all(requested.map(async operation => {
    try {
      const items = await host.query(operation, doc, position, q.severity ?? 'error');
      return { operation, status: items.length ? 'ok' : 'empty', items: items.slice(0, 200), omitted_at_least: Math.max(0, items.length - 200), truncated: items.length > 200 };
    } catch {
      // Provider exceptions do not prove lack of support and may include private extension state.
      return { operation, status: 'provider_error', items: [], omitted_at_least: 0, truncated: false };
    }
  }));
  if (doc.version() !== version) throw new QueryError('stale_document', 'The editor document changed during the query; query again.');
  return {
    status: 'ok',
    document: { uri: doc.uri, language_id: doc.languageId, version, dirty: doc.dirty(), position_encoding: 'utf-16' },
    capabilities: { hover: 'unknown', definition: 'unknown', references: 'unknown', diagnostics: 'unknown', document_symbols: 'unknown' },
    // VS Code's diagnostics collection does not expose a matching LSP document version.
    diagnostics_collection_available: true,
    diagnostics_freshness: 'editor_snapshot_unverified',
    results,
  };
}
