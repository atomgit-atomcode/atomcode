/** Activity comes from the session owner; timestamps alone are not a terminal
 * signal because a running turn can already have persisted checkpoints. */
export function browseActiveTurn(messages: readonly { role: string; ts?: number; retainedHistory?: boolean }[], active: boolean) {
  let user = -1;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === 'user' && !messages[i].retainedHistory) { user = i; break; }
  }
  let assistant = -1;
  if (active) {
    for (let i = messages.length - 1; i > user; i--) {
      if (messages[i].role === 'assistant') { assistant = i; break; }
    }
  }
  const started = user >= 0 ? messages[user].ts : undefined;
  return {
    assistant,
    startedAt: active && typeof started === 'number' && Number.isFinite(started) && started > 0 ? started : undefined,
  };
}
