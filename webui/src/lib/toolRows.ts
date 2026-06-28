// Tool-row model + the dedup primitive shared by the chat view.
//
// Tool calls render as rows under an assistant message. The bug this guards
// against: a tool_start event for one logical call (same `id`/call_id) can reach
// the handler more than once — e.g. a leaked /live broadcast subscription
// re-delivers the whole turn — and naive appending then shows the SAME call as
// several identical rows. Dedup is by `id` (the kernel call_id), so genuinely
// distinct calls (different ids, even same name+args across goal rounds) stay
// separate, while a replayed event is coalesced. Mirrors the tuix
// coalesce-by-call_id fix (commit 80d6540f).

export interface ToolRow {
  id: string;
  name: string;
  args: string;
  status: 'pending' | 'done' | 'error' | 'waiting_approval';
  duration_ms?: number;
  output?: string;
}

/** One ordered conversation segment: a run of assistant text, one tool call, or a
 *  non-fatal advisory notice (e.g. "conversation compacted"). Arrival order is
 *  preserved so the text→tool→notice interleaving matches the TUI. */
export type MsgPart =
  | { kind: 'text'; text: string }
  | { kind: 'tool'; tool: ToolRow }
  | { kind: 'notice'; text: string }
  | { kind: 'rate_limited'; text: string };

/**
 * Append `tool` as a new tool part, OR — when a tool part with the SAME `tool.id`
 * already exists — update that part in place (merging incoming fields over the
 * existing ones). Idempotent: replaying the same `tool_start` never adds a second
 * row. Pure; returns a new array (no mutation) for React state updates.
 */
export function upsertToolPart(parts: MsgPart[], tool: ToolRow): MsgPart[] {
  const idx = parts.findIndex((p) => p.kind === 'tool' && p.tool.id === tool.id);
  if (idx < 0) return [...parts, { kind: 'tool', tool }];
  const existing = (parts[idx] as { kind: 'tool'; tool: ToolRow }).tool;
  const next = parts.slice();
  next[idx] = { kind: 'tool', tool: { ...existing, ...tool } };
  return next;
}
