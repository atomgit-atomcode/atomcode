// Dedup for command-notice rows in the chat transcript.
//
// Recovery / active-check failures (`chat.recoveryBlocked`, `chat.activeCheckFailed`,
// `chat.cancelFailed`) re-fire on EVERY session load and EVERY blocked send attempt.
// Without dedup they stack a wall of identical rows — the reported "刷了一屏" when
// the daemon is unreachable and the user keeps hitting send. Suppress a notice that
// already sits in the CURRENT TRAILING RUN of notices; the run resets the moment a
// real (non-notice) message follows, so a genuine later repeat still shows.

export interface NoticeMessageLike {
  role: string;
  parts: { kind: string; text?: string }[];
}

/** Is `text` already present in the trailing run of notice rows?
 *
 * `insertBeforeBusyAssistant` mirrors the push path: while a streaming assistant
 * reply is the last element, a new notice is inserted BEFORE it, so the trailing
 * notice run is scanned starting just before that assistant tail. */
export function isDuplicateTrailingNotice(
  messages: NoticeMessageLike[],
  text: string,
  insertBeforeBusyAssistant: boolean,
): boolean {
  const scanEnd = insertBeforeBusyAssistant ? messages.length - 1 : messages.length;
  for (let i = scanEnd - 1; i >= 0; i--) {
    const m = messages[i];
    const isNotice = m.role === 'system' && m.parts.some((p) => p.kind === 'notice');
    if (!isNotice) break; // real content ends the trailing run
    if (m.parts.some((p) => p.kind === 'notice' && p.text === text)) return true;
  }
  return false;
}
