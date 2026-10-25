import type { RunEvent } from './scheduleApi';
export interface RunSummary {
  id: string; startedAt?: number; finishedAt?: number; status: string;
  prompt?: string; sessionId?: string; error?: string | null;
}
/** Append order is authoritative, even when the host clock moves backwards.
 * A page may begin halfway through a run; never substitute current instructions. */
export function foldScheduleRuns(events: RunEvent[]): RunSummary[] {
  const runs = new Map<string,RunSummary>();
  for (const event of events) {
    const run = runs.get(event.run_id) || {id:event.run_id,status:'running'};
    if (event.kind === 'started') { run.startedAt=event.at; run.prompt=event.prompt; }
    else if (event.kind === 'session') run.sessionId=event.session_id;
    else {run.finishedAt=event.at;run.status=event.status || 'unknown';run.error=event.error;}
    runs.set(event.run_id,run);
  }
  return [...runs.values()].reverse();
}
