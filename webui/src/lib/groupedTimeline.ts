import { boundedTimelineRange, type TimelineMessage } from './timelineMemory.ts';
import type { TurnProcess } from './turnProcess.ts';

/** Outer pages never cut a collapsed conversation in half. Expanded processes
 * retain a bounded inner window; only their user prompt and final answer are
 * pinned around it. Indexes still address the sole display/archive store. */
export function groupedTimelinePage(messages: readonly TimelineMessage[], size: number, start: number | null,
  budget: number, processes: ReadonlyMap<number, TurnProcess>, expanded: (process: TurnProcess) => boolean) {
  const range = boundedTimelineRange(messages, size, start, budget);
  const groups: { start: number; end: number }[] = [];
  for (let index = 0; index < messages.length;) {
    let end = index + 1;
    while (end < messages.length && (messages[end].role !== 'user'
      || (messages[end] as TimelineMessage & { retainedHistory?: boolean }).retainedHistory)) end++;
    groups.push({ start: index, end }); index = end;
  }
  const selected = new Set<number>();
  const processIndexes = new Set<number>();
  for (const group of groups) {
    if (group.end <= range.start || group.start >= range.end) continue;
    // No-process/simple turns are complete too, unless exceptionally large.
    let process = processes.get(group.end - 1);
    for (let index = group.start; !process && index < group.end; index++) process = processes.get(index);
    const collapsed = process?.foldable && !expanded(process);
    if (collapsed && process) {
      range.start = Math.min(range.start, group.start);
      range.end = Math.max(range.end, group.end);
      if (messages[group.start].role === 'user') selected.add(group.start);
      selected.add(process.last);
      // User history retained by compaction and independent notices are not
      // assistant process content and must never disappear into its disclosure.
      for (let index = group.start; index < group.end; index++) {
        if (messages[index].role !== 'assistant') selected.add(index);
      }
    } else {
      const from = Math.max(group.start, range.start), end = Math.min(group.end, range.end);
      for (let index = from; index < end; index++) { selected.add(index); processIndexes.add(index); }
      if (messages[group.start].role === 'user') selected.add(group.start);
      if (process?.foldable) selected.add(process.last);
      // Small adjacent rows, e.g. a user prompt followed by a simple answer,
      // should not become separate pages merely at the raw window boundary.
      if (!process && group.end - group.start <= 3) {
        range.start = Math.min(range.start, group.start); range.end = Math.max(range.end, group.end);
        for (let index = group.start; index < group.end; index++) selected.add(index);
      }
    }
  }
  return { range, indexes: [...selected].sort((a, b) => a - b), processIndexes };
}
