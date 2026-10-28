import { apiFetch } from './transport.ts';
import { getToken } from '../api.ts';
export type Rule = { kind: 'daily'; time: string } | { kind: 'weekly'; weekday: number; time: string } | { kind: 'hourly' } | { kind: 'interval'; every_minutes: number } | { kind: 'cron'; expr: string };
export interface ScheduledTask {
  id: string; title: string; prompt: string; cwd: string; schedule: Rule;
  permission_mode: string; notify: string; enabled: boolean; created_at: number;
  last_run_at?: number | null; last_status?: string | null;
}
export interface RunEvent {
  kind: 'started' | 'session' | 'finished'; run_id: string; at: number;
  task_id?: string; revision?: string; prompt?: string; cwd?: string;
  session_id?: string; status?: string; error?: string | null;
}
export interface ScheduleEntry {
  task: ScheduledTask; revision: string; registration: string; registration_error: string | null;
  next_run: { at: number; time_zone: string; estimated: boolean } | null;
  last_run: RunEvent | null; running: boolean;
}
export type ScheduleMutation = { action: 'create'; task: ScheduledTask } | { action: 'update'; task: ScheduledTask; expected: string } | { action: 'delete'; id: string; expected: string };
async function request<T>(path: string, body?: unknown, signal?: AbortSignal): Promise<T> {
  const headers: Record<string,string> = { 'X-AtomCode-Client': 'webui' };
  const token = getToken(); if (token) headers.Authorization = `Bearer ${token}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const response = await apiFetch(path, { method: body === undefined ? 'GET' : 'POST', headers, signal, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
  if (!response.ok) {
    const data = await response.json().catch(() => null);
    throw new Error(typeof data?.error === 'string' ? data.error : `Automation request failed (${response.status})`);
  }
  return response.json();
}
export async function listSchedules(signal?: AbortSignal) {
  const rows = await request<ScheduleEntry[]>('/schedules', undefined, signal);
  if (!Array.isArray(rows) || rows.some(row => !row?.task || typeof row.revision !== 'string' || typeof row.task.title !== 'string' || typeof row.task.prompt !== 'string' || typeof row.task.cwd !== 'string' || typeof row.task.enabled !== 'boolean' || !row.task.schedule)) throw new Error('Invalid automation catalog response');
  return rows;
}
export function scheduleHistory(id: string, signal?: AbortSignal, cursor?: number) { return request<{events:RunEvent[];next_cursor:number|null}>(`/schedules/${encodeURIComponent(id)}/history${cursor === undefined ? '' : '?cursor='+cursor}`, undefined, signal); }
export function changeSchedule(requestId: string, mutation: ScheduleMutation) { return request<{entry: ScheduleEntry | null; deleted_id: string | null; warning?: string | null}>('/schedules', { request_id: requestId, ...mutation }); }
export function runSchedule(id: string, requestId: string, expected: string) { return request<{state: string; error: string | null}>(`/schedules/${encodeURIComponent(id)}/run`, { request_id: requestId, expected }); }
export function reconcileSchedule(id: string) { return request<ScheduleEntry>(`/schedules/${encodeURIComponent(id)}/reconcile`, {}); }

/** Read-only subscription. Connection loss never causes a mutation replay. */
export async function watchSchedules(onChange: () => void, signal: AbortSignal): Promise<void> {
  const token = getToken();
  const response = await apiFetch('/schedules/events', { headers: { 'X-AtomCode-Client':'webui', ...(token ? {Authorization:`Bearer ${token}`} : {}) }, signal });
  if (!response.ok || !response.body || !response.headers.get('content-type')?.startsWith('text/event-stream')) throw new Error('Automation subscription unavailable');
  const reader = response.body.getReader();
  const decoder = new TextDecoder(); let buffered = '';
  try {
    while (!signal.aborted) {
      const {value,done} = await reader.read(); if (done) break;
      buffered += decoder.decode(value,{stream:true});
      let newline: number;
      while ((newline=buffered.indexOf('\n')) >= 0) {
        const line = buffered.slice(0,newline).replace(/\r$/, ''); buffered=buffered.slice(newline+1);
        if (line.startsWith('data:')) { const event=JSON.parse(line.slice(5)); if (event.error) throw new Error(String(event.error)); if(typeof event.revision === 'string') onChange(); }
      }
      if (buffered.length > 65536) throw new Error('Invalid automation subscription');
    }
  } finally { await reader.cancel().catch(()=>{}); reader.releaseLock(); }
}
