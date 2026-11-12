import type { MsgPart } from './toolRows.ts';

export interface ProcessMessage {
  role: 'user' | 'assistant' | 'system';
  parts: MsgPart[];
  ts?: number;
  sourceSeq?: number;
  turnAnchorId?: string;
  turnStop?: string;
  archiveKey?: string;
  retainedHistory?: boolean;
  processSummary?: ReturnType<typeof processSummary>;
}
/** Small, browser-only archive metadata. Never use preview text as an answer. */
export function processSummary(message: ProcessMessage) {
  let finalStart = 0;
  let tools = 0;
  let firstTool: string | undefined;
  let unsafe = Boolean(message.turnStop && !['finished', 'stopped', 'completed', 'end_turn', 'stop'].includes(message.turnStop));
  message.parts.forEach((part, index) => {
    if (part.kind !== 'text') finalStart = index + 1;
    if (part.kind === 'tool') {
      tools++; firstTool ??= part.tool.id;
      if (['pending', 'waiting_approval', 'incomplete'].includes(part.tool.status)) unsafe = true;
    } else if (part.kind !== 'text' || /\[(?:错误|连接错误|Error|Connection error):/.test(part.text)) unsafe = true;
  });
  return { tools, firstTool, unsafe, finalStart, parts: message.parts.length,
    hasAnswer: message.parts.slice(finalStart).some(part => part.kind === 'text' && Boolean(part.text.trim())) };
}
export interface TurnProcess {
  key: string;
  first: number;
  last: number;
  finalParts: MsgPart[];
  tools: number;
  steps: number;
  duration?: number;
  foldable: boolean;
}

/** Presentation only. Keep the last answer after the last tool outside the
 * disclosure; never infer a final answer from a tool-only or interrupted tail. */
export function turnProcesses(messages: readonly ProcessMessage[], running = false): Map<number, TurnProcess> {
  const result = new Map<number, TurnProcess>();
  let start = 0;
  while (start < messages.length) {
    let end = start + 1;
    while (end < messages.length && (messages[end].role !== 'user' || messages[end].retainedHistory)) end++;
    const indexes: number[] = [];
    let tools = 0;
    let firstTool: string | undefined;
    let unsafe = false;
    for (let i = start; i < end; i++) {
      const message = messages[i];
      if (message.role !== 'assistant') continue;
      indexes.push(i);
      const summary = message.processSummary ?? processSummary(message);
      tools += summary.tools;
      firstTool ??= summary.firstTool;
      unsafe ||= summary.unsafe;
    }
    const last = indexes[indexes.length - 1];
    if (last != null) {
      const tail = messages[last];
      const summary = tail.processSummary ?? processSummary(tail);
      const finalStart = summary.finalStart;
      const finalParts = tail.archiveKey ? [] : tail.parts.slice(finalStart);
      const hasAnswer = summary.hasAnswer && (!tail.archiveKey || Boolean(tail.processSummary));
      const hasProcess = indexes.length > 1 || finalStart > 0;
      if (hasProcess) {
        const user = messages[start].role === 'user' ? messages[start] : undefined;
        const group: TurnProcess = {
          key: `${user?.sourceSeq ?? user?.turnAnchorId ?? user?.ts ?? start}/${firstTool ?? 'text'}`,
          first: indexes[0], last, finalParts, tools, steps: indexes.reduce((count, index) => count + (index === last ? finalStart : (messages[index].processSummary?.parts ?? messages[index].parts.length)), 0),
          duration: user?.ts != null && tail.ts != null ? Math.max(0, tail.ts - user.ts) : undefined,
          foldable: hasAnswer && !unsafe && !(running && (end === messages.length || tail.ts == null)),
        };
        for (const index of indexes) result.set(index, group);
      }
    }
    start = end;
  }
  return result;
}
