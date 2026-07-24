import type { ChatMessage, MessageBlock, SearchMatch, SearchMatchRange } from '../state/types';
import { blocksFromLegacyMessage } from '../state/blocks';

/** Re-exported so existing imports keep working. */
export type { SearchMatch, SearchMatchRange };

/**
 * Collect a single, linear searchable string for a message.
 *
 * IMPORTANT: this MUST cover only text that the UI can actually HIGHLIGHT,
 * otherwise the `{current}/{total}` counter and the dim/focus feedback count
 * "phantom" matches that render no visible `<mark>` — landing the user on a
 * message with nothing highlighted (the mislabel the search feature is meant
 * to avoid). Only `text` and `artifact` blocks flow through
 * `Markdown.tsx → highlightHtml`; `tool` (args/output) and `status` blocks have
 * NO highlight path, so they are deliberately EXCLUDED here. (If tool/status
 * search coverage is ever wanted back, it must come WITH a highlight path in
 * those block renderers, so count and highlights stay in sync.)
 */
export function getMessageSearchableText(message: ChatMessage): string {
  if (message.role === 'user' || message.role === 'error') {
    return message.text ?? '';
  }

  const blocks: MessageBlock[] = message.blocks && message.blocks.length > 0
    ? message.blocks
    : blocksFromLegacyMessage(message);

  const parts: string[] = [];
  for (const block of blocks) {
    if (block.type === 'text') {
      parts.push(block.content);
    } else if (block.type === 'artifact') {
      parts.push(block.artifact.content);
    }
    // `tool` / `status` blocks are intentionally NOT searched: they have no
    // highlight path, so matching them would produce phantom (uncounted-yet-
    // unhighlighted) hits. See the doc comment above.
  }
  return parts.join('\n');
}

/** Case-insensitive substring match. Returns all ranges in document order. */
export function findMatches(text: string, query: string): SearchMatchRange[] {
  const trimmed = query.trim();
  if (!trimmed) return [];
  const lowerText = text.toLowerCase();
  const lowerQuery = trimmed.toLowerCase();
  const ranges: SearchMatchRange[] = [];
  let from = 0;
  while (true) {
    const idx = lowerText.indexOf(lowerQuery, from);
    if (idx < 0) break;
    ranges.push({ start: idx, length: lowerQuery.length });
    from = idx + lowerQuery.length;
    if (ranges.length > 500) break; // hard cap to protect rendering perf
  }
  return ranges;
}

/**
 * Build all matches across a list of messages, in display order.
 * Each entry groups the ranges for a single message so the UI can
 * highlight multiple hits inside one bubble.
 */
export function buildSearchMatches(
  messages: ChatMessage[],
  query: string,
): SearchMatch[] {
  if (!query.trim()) return [];
  const matches: SearchMatch[] = [];
  for (const msg of messages) {
    const ranges = findMatches(getMessageSearchableText(msg), query);
    if (ranges.length > 0) {
      matches.push({ messageId: msg.id, ranges });
    }
  }
  return matches;
}

const HTML_ENTITIES: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
};

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (ch) => HTML_ENTITIES[ch] ?? ch);
}

/**
 * Highlight occurrences of `query` inside a plain-text string and return
 * HTML with <mark class="search-highlight"> wrappers. Used by the user
 * message bubble which renders text without markdown processing.
 */
export function highlightPlainText(text: string, query: string): string {
  if (!query.trim()) return escapeHtml(text);
  const ranges = findMatches(text, query);
  if (ranges.length === 0) return escapeHtml(text);

  let html = '';
  let cursor = 0;
  for (const range of ranges) {
    if (range.start > cursor) {
      html += escapeHtml(text.slice(cursor, range.start));
    }
    html += `<mark class="search-highlight">`;
    html += escapeHtml(text.slice(range.start, range.start + range.length));
    html += `</mark>`;
    cursor = range.start + range.length;
  }
  if (cursor < text.length) {
    html += escapeHtml(text.slice(cursor));
  }
  return html;
}

const TEXT_NODE_TAG_PATTERN = /^<(?:p|li|td|th|h[1-6]|blockquote|strong|em|code|pre|span|div|a|ul|ol|table|thead|tbody|tr|article|section|dd|dt|dl)(\s|>|\/)/i;

interface QueryVariant {
  /** The string to match against the HTML. */
  text: string;
  /** Whether this is already entity-encoded (skip re-escaping inside mark). */
  alreadyEscaped: boolean;
}

/**
 * Build match variants for the query: the raw form and the entity-encoded
 * form. `buildSearchMatches` runs against raw text, but rendered HTML has
 * `&` → `&amp;` etc., so we also need to match the encoded version.
 */
function buildQueryVariants(query: string): QueryVariant[] {
  const trimmed = query.trim();
  const escaped = escapeHtml(trimmed);
  if (escaped === trimmed) return [{ text: trimmed, alreadyEscaped: false }];
  return [
    { text: trimmed, alreadyEscaped: false },
    { text: escaped, alreadyEscaped: true },
  ];
}

/**
 * Inject <mark class="search-highlight"> into already-rendered HTML.
 *
 * Only literal text content is touched — we never highlight inside tags,
 * attribute values, or <pre>/<code> blocks (where escaped source lives).
 * The resulting HTML must still be passed through DOMPurify.sanitize().
 */
export function highlightHtml(html: string, query: string): string {
  const trimmed = query.trim();
  if (!trimmed) return html;

  const variants = buildQueryVariants(trimmed);
  // Sort by length descending so the longest (escaped) variant is tried first.
  variants.sort((a, b) => b.text.length - a.text.length);

  let result = '';
  let i = 0;
  let inTag = false;
  let inCodeBlock = false;
  let codeDepth = 0;

  while (i < html.length) {
    const ch = html[i];

    if (inTag) {
      result += ch;
      if (ch === '>') {
        inTag = false;
      }
      i += 1;
      continue;
    }

    if (ch === '<') {
      // Detect closing tags so we can track nested code/pre blocks.
      const rest = html.slice(i);
      const closeMatch = /^<\/([a-zA-Z][\w-]*)\s*>/.exec(rest);
      const openMatch = /^<([a-zA-Z][\w-]*)/.exec(rest);

      if (closeMatch) {
        const name = closeMatch[1].toLowerCase();
        if (name === 'code' || name === 'pre') {
          codeDepth = Math.max(0, codeDepth - 1);
          inCodeBlock = codeDepth > 0;
        }
        result += closeMatch[0];
        i += closeMatch[0].length;
        continue;
      }

      if (openMatch) {
        const name = openMatch[1].toLowerCase();
        if ((name === 'code' || name === 'pre') && TEXT_NODE_TAG_PATTERN.test(rest)) {
          codeDepth += 1;
          inCodeBlock = codeDepth > 0;
        }
        result += '<';
        inTag = true;
        i += 1;
        continue;
      }

      // Unknown tag-like sequence, copy as-is.
      result += ch;
      i += 1;
      continue;
    }

    if (inCodeBlock) {
      result += ch;
      i += 1;
      continue;
    }

    // Try to match each query variant at the current position (longest first).
    let matched = false;
    for (const variant of variants) {
      const slice = html.slice(i, i + variant.text.length);
      if (slice.length >= variant.text.length && slice.toLowerCase() === variant.text.toLowerCase()) {
        result += variant.alreadyEscaped
          ? `<mark class="search-highlight">${slice}</mark>`
          : `<mark class="search-highlight">${escapeHtml(slice)}</mark>`;
        i += variant.text.length;
        matched = true;
        break;
      }
    }
    if (matched) continue;

    result += ch;
    i += 1;
  }

  return result;
}
