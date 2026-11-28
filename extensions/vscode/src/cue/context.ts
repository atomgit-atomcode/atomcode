/**
 * The context one completion request carries — and the rules that shape it.
 *
 * This file deliberately does NOT import `vscode`. The editor-facing code
 * lives in `provider.ts`; the rules about WHAT the model sees live here, so
 * they can be tested without an editor (the same split `languageBridge`
 * uses: `query.ts` holds the protocol, `vscode.ts` holds the adapter).
 */

/** The most text sent to the daemon as "before the cursor". */
export const MAX_PREFIX_CHARS = 8000;

/** The most text sent to the daemon as "after the cursor". */
export const MAX_SUFFIX_CHARS = 4000;

/** Files bigger than this are not completed — the model would be guessing. */
export const MAX_DOCUMENT_CHARS = 400_000;

/**
 * Languages CUE does not try to complete.
 *
 * These are the files where a suggestion is either meaningless or actively
 * harmful: a data file, a config file, or prose. Completing them wastes the
 * user's quota on text nobody reads.
 */
export const EXCLUDED_LANGUAGES = new Set([
  'json',
  'jsonc',
  'json5',
  'yaml',
  'xml',
  'markdown',
  'log',
  'toml',
  'ini',
  'csv',
  'plaintext',
]);

/** The text around the cursor. */
export interface CompletionContext {
  /** The document text before the cursor. */
  prefix: string;
  /** The document text after the cursor. */
  suffix: string;
}

/**
 * Split the document into the text before and after the cursor.
 *
 * The caller supplies the whole text and the cursor's offset, because that is
 * what the editor has — and because a rule that depends on the editor's
 * object shape cannot be tested here.
 */
export function buildCompletionContext(text: string, offset: number): CompletionContext {
  if (text.length > MAX_DOCUMENT_CHARS) {
    return { prefix: '', suffix: '' };
  }
  const at = Math.max(0, Math.min(offset, text.length));
  return {
    prefix: text.slice(0, at),
    suffix: text.slice(at),
  };
}

/**
 * Trim the prefix to its tail and the suffix to its head.
 *
 * The prefix keeps its TAIL (the code just above the cursor is what predicts
 * it) and the suffix keeps its HEAD (the code just below shows where the
 * block ends).
 *
 * The cut lands on a CHARACTER boundary, never inside a surrogate pair: a
 * half a character would reach the model as a replacement character, and the
 * model would answer around a character that is not in the file.
 */
export function trimContext(context: CompletionContext): CompletionContext {
  return {
    prefix: tail(context.prefix, MAX_PREFIX_CHARS),
    suffix: head(context.suffix, MAX_SUFFIX_CHARS),
  };
}

/** The last `limit` characters of `text`, cut on a character boundary. */
function tail(text: string, limit: number): string {
  if (text.length <= limit) return text;
  let start = text.length - limit;
  // A low surrogate means the cut landed inside a pair: step back one unit.
  if (start > 0 && isLowSurrogate(text.charCodeAt(start))) start -= 1;
  return text.slice(start);
}

/** The first `limit` characters of `text`, cut on a character boundary. */
function head(text: string, limit: number): string {
  if (text.length <= limit) return text;
  let end = limit;
  // A high surrogate means the cut would split a pair: step back one unit.
  if (isHighSurrogate(text.charCodeAt(end - 1))) end -= 1;
  return text.slice(0, end);
}

function isHighSurrogate(code: number): boolean {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code: number): boolean {
  return code >= 0xdc00 && code <= 0xdfff;
}

/**
 * Whether a language is worth completing at all.
 *
 * Kept separate from the document check so the rule can be read (and
 * changed) on its own.
 */
export function isCompletableLanguage(languageId: string): boolean {
  return !EXCLUDED_LANGUAGES.has(languageId);
}

/**
 * Whether a document is worth completing.
 *
 * An empty document is not: there is no context to complete from, and the
 * model's answer would be pure invention.
 */
export function isCompletableDocument(
  languageId: string,
  scheme: string,
  text: string,
): boolean {
  if (!isCompletableLanguage(languageId)) return false;
  if (scheme !== 'file') return false;
  if (text.trim().length === 0) return false;
  return true;
}
