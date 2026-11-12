import type { ImageData, SessionMessage } from '../api';

const INTERNAL_USER_PREFIXES = [
  '<system-reminder>',
  'You made code edits but have not verified them.',
  'Output limit hit — your last response was cut off',
  'Output limit hit. If the task is already complete',
  '[PLAN MODE',
  '[Context was compressed',
  '[Additional context from user]:',
  '[SYNTAX CHECK:',
  '[DEV SERVER ERROR',
  '[Auto-read from error:',
  '[Images returned by the tool calls above',
];

export function isInternalHistoryUserMessage(text: string, synthetic?: boolean): boolean {
  if (synthetic === true) return true;
  const trimmed = text.trimStart();
  return INTERNAL_USER_PREFIXES.some((prefix) => trimmed.startsWith(prefix));
}

export function isInternalHistoryAssistantMessage(msg: SessionMessage): boolean {
  const internalOrigin = msg.internal_origin ?? msg.internalOrigin;
  return msg.role === 'assistant'
    && internalOrigin === 'verify_cadence'
    && !(msg.tool_calls?.length);
}

export function isUserInterruptionMessage(msg: SessionMessage): boolean {
  const internalOrigin = msg.internal_origin ?? msg.internalOrigin;
  return internalOrigin === 'atomcode.user_interruption'
    || (msg.synthetic === true
      && msg.role === 'user'
      && msg.content.includes('interrupted by the user before completing'));
}

export function shouldShowAssistantTimestamp(
  isLastInTurn: boolean,
  streaming: boolean,
  text: string,
  isError: boolean,
): boolean {
  return isLastInTurn && !streaming && text.length > 0 && !isError;
}

/** Return one flag per timeline message. System notices are transparent and only
 * a real user message closes the preceding assistant turn. */
export function assistantTurnEndFlags(roles: readonly string[]): boolean[] {
  const result = roles.map(() => false);
  let lastAssistant = -1;

  for (let index = 0; index < roles.length; index += 1) {
    if (roles[index] === 'assistant') {
      lastAssistant = index;
    } else if (roles[index] === 'user' && lastAssistant >= 0) {
      result[lastAssistant] = true;
      lastAssistant = -1;
    }
  }
  if (lastAssistant >= 0) result[lastAssistant] = true;
  return result;
}

export function sessionMessagesToMarkdownLines(
  messages: SessionMessage[],
  title: string,
): string[] {
  const lines: string[] = [`# ${title}`, ''];
  for (const msg of messages) {
    if (msg.role === 'system') continue;
    if (msg.role === 'user') {
      if (isInternalHistoryUserMessage(msg.content || '', msg.synthetic)) continue;
      lines.push('## User', '', msg.content || '', '');
    } else if (msg.role === 'assistant') {
      if (isInternalHistoryAssistantMessage(msg)) continue;
      lines.push('## Assistant', '');
      if (msg.content) {
        lines.push(msg.content, '');
      }
      if (msg.tool_calls && msg.tool_calls.length > 0) {
        for (const tc of msg.tool_calls) {
          lines.push(`### Tool: ${tc.name}`, '');
          if (tc.arguments) {
            lines.push('```json', tc.arguments, '```', '');
          }
        }
      }
    } else if (msg.role === 'tool' && msg.tool_result) {
      const tr = msg.tool_result;
      lines.push(`### Tool Result (${tr.success ? '✓' : '✗'})`, '');
      if (tr.summary) {
        lines.push(tr.summary, '');
      }
    }
  }
  return lines;
}

/** Provenance is explicit; matching text alone must never reclassify a user. */
export function isRetainedHistoryUserMessage(msg: SessionMessage): boolean {
  return msg.role === 'user' && (msg.internal_origin ?? msg.internalOrigin) === 'compaction_retained_user';
}

/** Display only: captions are model context, never the person's typed prompt. */
export function displayHistoryUser(text: string, images?: ImageData[]): { text: string; images?: ImageData[] } {
  const markers = ['[图片内容（由', '[图片识别失败]', '[what is in the picture, read by ', '[the picture could not be read]'];
  const positions = markers.map(m => text.indexOf(m)).filter(i => i >= 0);
  const cut = positions.length ? Math.min(...positions) : -1;
  let prompt = cut >= 0 ? text.slice(0, cut).trimEnd() : text;
  const actual = images?.filter(image => !image.missing && image.data.length > 0);
  const missing = images?.filter(image => image.missing || !image.data).length ?? (cut >= 0 ? 1 : 0);
  if (missing && !/\[Image #\d+\]/i.test(prompt)) {
    prompt = [prompt, ...Array.from({ length: missing }, (_, i) => `[Image #${i + 1}]`)].filter(Boolean).join(' ');
  }
  return { text: prompt, images: actual?.length ? actual : undefined };
}

/** Hide only a redundant trailing attachment list, never inline image references
 * or markers whose corresponding payload is unavailable. Source text stays intact. */
export function displayImageMarkers(text: string, images?: readonly ImageData[]): string {
  const count = images?.length ?? 0;
  if (!count || images?.some(image => image.missing || !image.data.length)) return text;
  const suffix = text.match(/(?:\s*\[Image #\d+\])+\s*$/i);
  if (!suffix) return text;
  const markers = [...suffix[0].matchAll(/\[Image #(\d+)\]/gi)].map(match => Number(match[1]));
  if (markers.length !== count || markers.some((number, index) => number !== index + 1)) return text;
  return text.slice(0, suffix.index).trimEnd();
}
