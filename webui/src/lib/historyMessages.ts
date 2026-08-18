import type { SessionMessage } from '../api';

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
