import type { MsgKey } from '../i18n.ts';
import type { MsgPart, ToolRow } from './toolRows.ts';

interface BackgroundPresentation {
  title?: MsgKey;
  detail?: string;
  cls: 'pending' | 'background' | 'neutral' | 'success' | 'error' | 'warning';
  label: MsgKey;
  params?: Record<string, string | number>;
}

function argsOf(tool: ToolRow): Record<string, unknown> | undefined {
  try {
    const args: unknown = JSON.parse(tool.args);
    return args !== null && typeof args === 'object' && !Array.isArray(args)
      ? args as Record<string, unknown> : undefined;
  } catch { return undefined; }
}

function safeJobId(value: unknown): string | undefined {
  return typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/.test(value)
    ? value : undefined;
}

/** Presentation only: a successful status read is not a successful process exit.
 * Inspect the final status line only for our known poll tool, never arbitrary
 * output from file reads, shell commands or MCP tools. Unknown formats stay neutral. */
export function backgroundPresentation(tool: ToolRow): BackgroundPresentation | undefined {
  if (tool.status !== 'pending' && tool.status !== 'done') return undefined;
  if (!['bash_poll', 'bash_start', 'pwsh_start', 'bash_kill', 'team'].includes(tool.name)) return undefined;
  const args = argsOf(tool);
  if (tool.name === 'bash_poll') {
    const common = { title: 'tool.backgroundPoll' as const, detail: safeJobId(args?.job_id) };
    if (tool.status === 'pending') return { ...common, cls: 'pending', label: 'tool.pollWaiting' };
    const output = (tool.output ?? '').trimEnd();
    const status = output.slice(output.lastIndexOf('\n') + 1).trim();
    if (status === 'status: running') return { ...common, cls: 'background', label: 'tool.jobRunning' };
    if (status === 'status: killed') return { ...common, cls: 'warning', label: 'tool.jobStopped' };
    // A shell may have exited while descendants still hold its output pipe.
    if (/^status: (?:running|exited \(code -?\d+\)|killed); what it started is still running and writing here — keep polling, or stop it with bash_kill$/.test(status ?? '')) {
      return { ...common, cls: 'background', label: 'tool.jobChildrenRunning' };
    }
    const exited = /^status: exited \(code (-?\d+)\)$/.exec(status ?? '');
    if (exited) return { ...common, cls: exited[1] === '0' ? 'success' : 'error', label: 'tool.jobExited', params: { code: exited[1] } };
    return { ...common, cls: 'neutral', label: 'tool.pollReturned' };
  }
  if (tool.name === 'bash_start' || tool.name === 'pwsh_start') {
    const common = { title: tool.name === 'pwsh_start' ? 'tool.backgroundPowerShell' as const : 'tool.backgroundBash' as const,
      detail: typeof args?.command === 'string' ? args.command : undefined };
    if (tool.status === 'pending') return { ...common, cls: 'pending', label: 'tool.jobStarting' };
    const started = /^Started background job ([a-zA-Z0-9._-]+)\. Collect output with bash_poll /.exec(tool.output ?? '');
    return { ...common, detail: started ? `${started[1]} · ${common.detail ?? ''}`.trim() : common.detail,
      cls: 'neutral', label: started ? 'tool.jobStarted' : 'tool.requestProcessed' };
  }
  if (tool.name === 'bash_kill') {
    return { title: 'tool.backgroundStop', detail: safeJobId(args?.job_id), cls: tool.status === 'pending' ? 'pending' : 'neutral',
      label: tool.status === 'pending' ? 'tool.jobStopping' : 'tool.stopRequestProcessed' };
  }
  if (tool.name === 'team' && tool.status === 'done') {
    const labels = { delegate: 'tool.teamDelegated', tell: 'tool.teamSent', status: 'tool.pollReturned', stop: 'tool.stopRequestProcessed' } as const;
    const action = typeof args?.action === 'string' ? args.action : '';
    if (Object.prototype.hasOwnProperty.call(labels, action)) return { cls: 'neutral', label: labels[action as keyof typeof labels],
      detail: [action, typeof args?.name === 'string' ? args.name : ''].filter(Boolean).join(' · ') };
  }
  return undefined;
}

export interface PollGroups {
  /** First call id is stable while subsequent polling rounds arrive. */
  byFirstId: Map<string, ToolRow[]>;
  hidden: Set<string>;
}

function emptyPollJob(tool: ToolRow): string | undefined {
  if (tool.name !== 'bash_poll' || tool.progress?.trim()) return undefined;
  if (tool.status === 'done' ? tool.output?.trim() !== 'status: running'
    : tool.status !== 'pending' || Boolean(tool.output?.trim())) return undefined;
  return safeJobId(argsOf(tool)?.job_id);
}

/** Collapse presentation across adjacent assistant rounds only. The original
 * messages, call ids, result statuses and outputs are retained without mutation. */
export function groupEmptyPolls(messages: readonly { role: string; parts: readonly MsgPart[] }[]): PollGroups {
  const groups: PollGroups = { byFirstId: new Map(), hidden: new Set() };
  let streak: ToolRow[] = [];
  let job: string | undefined;
  const flush = () => {
    if (streak.length > 1) {
      groups.byFirstId.set(streak[0].id, streak);
      for (const tool of streak.slice(1)) groups.hidden.add(tool.id);
    }
    streak = [];
    job = undefined;
  };
  for (const message of messages) {
    if (message.role !== 'assistant') { flush(); continue; }
    for (const part of message.parts) {
      if (part.kind === 'text' && !part.text.trim()) continue;
      const next = part.kind === 'tool' ? emptyPollJob(part.tool) : undefined;
      if (!next || next !== job) flush();
      if (next && part.kind === 'tool') { job = next; streak.push(part.tool); }
    }
  }
  flush();
  return groups;
}
