import type { Rule, ScheduleEntry, ScheduledTask } from './scheduleApi.ts';

const editable = ['title', 'prompt', 'cwd', 'schedule', 'permission_mode', 'notify', 'enabled'] as const;
type Field = typeof editable[number];
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value !== null && typeof value === 'object') return `{${Object.entries(value).sort(([a],[b])=>a.localeCompare(b)).map(([key,v])=>`${JSON.stringify(key)}:${canonical(v)}`).join(',')}}`;
  return JSON.stringify(value) ?? 'undefined';
}
export function sameTask(a: ScheduledTask, b: ScheduledTask): boolean {
  return editable.every(key => canonical(a[key]) === canonical(b[key]));
}
/** Refresh untouched fields; never silently overwrite a field changed by both editors. */
export function rebaseSchedule(base: ScheduleEntry, draft: ScheduledTask, incoming: ScheduleEntry): { draft: ScheduledTask; conflicts: Field[] } {
  const result = structuredClone(incoming.task);
  const conflicts: Field[] = [];
  for (const key of editable) {
    if (canonical(draft[key]) === canonical(base.task[key])) continue;
    if (canonical(incoming.task[key]) !== canonical(base.task[key]) && canonical(incoming.task[key]) !== canonical(draft[key])) conflicts.push(key);
    Object.assign(result, { [key]: structuredClone(draft[key]) });
  }
  return { draft: result, conflicts };
}
/** Published task files may use 9:0; display the equivalent HTML time value. */
export function clockInputValue(time: string): string {
  const match = /^(\d{1,2}):(\d{1,2})$/.exec(time);
  if (!match || Number(match[1]) > 23 || Number(match[2]) > 59) return time;
  return `${match[1].padStart(2, '0')}:${match[2].padStart(2, '0')}`;
}
/** Switching between calendar rules keeps the time the person already chose. */
export function selectRule(current: Rule, kind: string): Rule {
  if (current.kind === kind) return current;
  const time = current.kind === 'daily' || current.kind === 'weekly' ? current.time : '09:00';
  if (kind === 'daily') return { kind, time };
  if (kind === 'weekly') return { kind, weekday: 1, time };
  if (kind === 'hourly') return { kind };
  if (kind === 'interval') return { kind, every_minutes: 60 };
  return current;
}
export type FormProblem = 'title' | 'prompt' | 'time' | 'weekday' | 'interval' | 'cron';
export function scheduleProblem(task: ScheduledTask): FormProblem | null {
  if (!task.title.trim() || [...task.title].length > 120) return 'title';
  if (!task.prompt.trim() || new TextEncoder().encode(task.prompt).length > 65536) return 'prompt';
  const rule = task.schedule;
  if (rule.kind === 'cron') return 'cron';
  if (rule.kind === 'interval' && (!Number.isSafeInteger(rule.every_minutes) || rule.every_minutes < 1 || rule.every_minutes > 4294967295)) return 'interval';
  if ((rule.kind === 'daily' || rule.kind === 'weekly') && !/^([01]\d|2[0-3]):[0-5]\d$/.test(clockInputValue(rule.time))) return 'time';
  if (rule.kind === 'weekly' && (!Number.isInteger(rule.weekday) || rule.weekday < 1 || rule.weekday > 7)) return 'weekday';
  return null;
}
export function orderedSchedules(rows: ScheduleEntry[], query: string, filter: string): ScheduleEntry[] {
  const needle = query.trim().toLocaleLowerCase();
  return rows.filter(row => (filter === 'all' || row.task.enabled === (filter === 'enabled')) && `${row.task.title} ${row.task.prompt} ${row.task.cwd}`.toLocaleLowerCase().includes(needle))
    .sort((a,b) => Number(b.task.enabled)-Number(a.task.enabled) || (a.next_run?.at ?? Infinity)-(b.next_run?.at ?? Infinity) || a.task.title.localeCompare(b.task.title) || a.task.id.localeCompare(b.task.id));
}
