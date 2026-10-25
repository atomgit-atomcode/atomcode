import { useEffect, useRef, useState } from 'preact/hooks';
import { useSettings } from '../settings';
import { randomId } from '../lib/randomId';
import { watchSchedules, changeSchedule, listSchedules, reconcileSchedule, runSchedule, scheduleHistory, type Rule, type RunEvent, type ScheduleEntry, type ScheduledTask, type ScheduleMutation } from '../lib/scheduleApi';
import { selectRule, clockInputValue, orderedSchedules, rebaseSchedule, sameTask, scheduleProblem, type FormProblem } from '../lib/scheduleForm';
import { foldScheduleRuns } from '../lib/scheduleHistory';
import { ConfirmDialog } from './ConfirmDialog';
import './Automation.css';

export function ScheduleIcon() {
  return <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><circle cx="12" cy="12" r="8.5"/><path d="M12 7v5l3.5 2" stroke-linecap="round"/></svg>;
}
interface Props {
  cwd: string;
  onClose: () => void;
  onSession: (id: string, isCurrent?: () => boolean) => Promise<void>;
  onCreateInChat?: () => Promise<void>;
}

export function Automation({ cwd, onClose, onSession, onCreateInChat }: Props) {
  const { lang } = useSettings();
  const text = (zh: string, en: string) => lang === 'zh' ? zh : en;
  const [entries, setEntries] = useState<ScheduleEntry[]>([]);
  const [settled, setSettled] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState('all');
  const [selected, setSelected] = useState<ScheduleEntry | null>(null);
  const [draft, setDraft] = useState<ScheduledTask | null>(null);
  const [tab, setTab] = useState<'rule' | 'history'>('rule');
  const [history, setHistory] = useState<RunEvent[]>([]);
  const [historyCursor, setHistoryCursor] = useState<number | null>(null);
  const [historyError, setHistoryError] = useState('');
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyReload, setHistoryReload] = useState(0);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const [deleteConfirm, setDeleteConfirm] = useState<ScheduleEntry | null>(null);
  const [deletedId, setDeletedId] = useState<string | null>(null);
  const [discardAction, setDiscardAction] = useState<(() => void) | null>(null);
  const dialog = useRef<HTMLDivElement>(null);
  const historyController = useRef<AbortController | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const initialDraft = useRef<ScheduledTask | null>(null);
  const lifetime = useRef(0);
  const view = useRef(0);
  const readSequence = useRef(0);
  const mutationRequest = useRef<{ signature: string; id: string } | null>(null);
  const runRequest = useRef<{ taskId: string; revision: string; id: string } | null>(null);
  const alive = (life: number, seat: number) => lifetime.current === life && view.current === seat;
  const latest = entries.find(entry => entry.task.id === selected?.task.id);
  const missing = Boolean(settled && selected && !latest);
  const current = latest || selected;
  const dirty = draft !== null && !sameTask(selected?.task || initialDraft.current || draft, draft);
  const rebased = selected && draft && latest && selected.revision !== latest.revision ? rebaseSchedule(selected, draft, latest) : null;
  const conflicting = Boolean(rebased?.conflicts.length);
  const problem = draft ? scheduleProblem(draft) : null;
  const visible = orderedSchedules(entries, query, filter);
  // Keep an open editor visible even when a refresh changes its filter membership.
  const rows = selected && !visible.some(row => row.task.id === selected.task.id) ? [latest || selected, ...visible] : visible;
  const fresh = (): ScheduledTask => ({ id: randomId(), title: '', prompt: '', cwd, schedule: { kind: 'daily', time: '09:00' }, permission_mode: 'plan', notify: 'important', enabled: true, created_at: 0 });

  async function refresh(signal?: AbortSignal): Promise<ScheduleEntry[] | null> {
    const life = lifetime.current, read = ++readSequence.current;
    setLoading(true);
    try {
      const rows = await listSchedules(signal);
      if (life !== lifetime.current || read !== readSequence.current || signal?.aborted) return null;
      setEntries(rows); setSettled(true); setLoadError('');
      return rows;
    } catch (error) {
      if (life === lifetime.current && read === readSequence.current && !signal?.aborted) setLoadError(String(error));
      return null;
    } finally {
      if (life === lifetime.current && read === readSequence.current && !signal?.aborted) setLoading(false);
    }
  }
  useEffect(() => {
    const controller = new AbortController();
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.querySelector<HTMLButtonElement>('button')?.focus();
    const update = () => { if (document.visibilityState !== 'hidden') void refresh(controller.signal); };
    update();
    void watchSchedules(update, controller.signal).catch(() => { /* Foreground reads and polling recover a lost subscription. */ });
    const timer = window.setInterval(update, 10000);
    window.addEventListener('online', update); document.addEventListener('visibilitychange', update);
    return () => { lifetime.current++; controller.abort(); historyController.current?.abort(); clearInterval(timer); window.removeEventListener('online', update); document.removeEventListener('visibilitychange', update); previous?.focus(); };
  }, []);
  useEffect(() => {
    if (rebased && !rebased.conflicts.length && latest) { setSelected(latest); setDraft(rebased.draft); }
  }, [entries, selected?.revision, draft]);
  useEffect(() => {
    if (deletedId && settled && !entries.some(entry => entry.task.id === deletedId)) { closeEditor(); setDeletedId(null); }
  }, [entries, deletedId, settled]);
  useEffect(() => {
    if (tab !== 'history' || !selected) return;
    const controller = new AbortController(), life = lifetime.current, seat = view.current;
    historyController.current?.abort(); historyController.current = controller;
    setHistoryError(''); setHistoryLoading(true);
    scheduleHistory(selected.task.id, controller.signal).then(page => {
      if (alive(life, seat) && !controller.signal.aborted) { setHistory(page.events); setHistoryCursor(page.next_cursor); }
    }).catch(error => { if (alive(life, seat) && !controller.signal.aborted) setHistoryError(String(error)); })
      .finally(() => { if (alive(life, seat) && !controller.signal.aborted) setHistoryLoading(false); });
    return () => { historyController.current?.abort(); };
  }, [tab, selected?.task.id, JSON.stringify(current?.last_run), historyReload]);

  function closeEditor() {
    view.current++; historyController.current?.abort(); setBusy(false); setSelected(null); setDraft(null); setDeleteConfirm(null);
    setHistory([]); setHistoryCursor(null); setHistoryError(''); setHistoryLoading(false);
    mutationRequest.current = null; runRequest.current = null; initialDraft.current = null;
    requestAnimationFrame(() => { if (opener.current?.isConnected) opener.current.focus(); else dialog.current?.querySelector<HTMLButtonElement>('button')?.focus(); });
  }
  function navigate(action: () => void) { if (dirty) setDiscardAction(() => action); else action(); }
  function open(entry: ScheduleEntry | null) {
    closeEditor(); opener.current = document.activeElement as HTMLElement | null;
    const value = entry ? structuredClone(entry.task) : fresh();
    initialDraft.current = structuredClone(value);
    setSelected(entry); setDraft(value); setTab('rule'); setNotice('');
    requestAnimationFrame(() => dialog.current?.querySelector<HTMLElement>(entry ? '.automation-editor-heading' : '.automation-editor input')?.focus());
  }
  function edit(patch: Partial<ScheduledTask>) { if (draft) setDraft({ ...draft, ...patch }); }
  function requestId(mutation: ScheduleMutation): string {
    const signature = JSON.stringify(mutation);
    if (mutationRequest.current?.signature !== signature) mutationRequest.current = { signature, id: randomId() };
    return mutationRequest.current.id;
  }
  async function save() {
    if (!draft || problem || conflicting || missing || busy) return;
    const mutation: ScheduleMutation = selected ? { action: 'update', task: draft, expected: selected.revision } : { action: 'create', task: draft };
    const life = lifetime.current, seat = view.current; setBusy(true); setNotice('');
    try {
      const result = await changeSchedule(requestId(mutation), mutation);
      if (!alive(life, seat)) return;
      if (result.entry) {
        const entry = result.entry;
        setEntries(rows => [entry, ...rows.filter(row => row.task.id !== entry.task.id)]);
        setSelected(entry); setDraft(structuredClone(entry.task));
        setNotice(entry.registration_error || text('已保存', 'Saved'));
      }
      await refresh();
    } catch (error) {
      if (alive(life, seat)) { setNotice(`${String(error)} · ${text('草稿已保留，请核对最新任务后重试。', 'Your draft is preserved. Check the latest task before retrying.')}`); await refresh(); }
    } finally { if (alive(life, seat)) setBusy(false); }
  }
  async function remove() {
    if (!deleteConfirm) return;
    // The confirmation owns the observed revision, even if polling updates the editor.
    const life = lifetime.current, seat = view.current, id = deleteConfirm.task.id;
    const mutation: ScheduleMutation = { action: 'delete', id, expected: deleteConfirm.revision };
    setBusy(true);
    try {
      const result = await changeSchedule(requestId(mutation), mutation);
      if (!alive(life, seat)) return;
      setDeletedId(id); setNotice(result.warning || text('任务已删除，运行记录和会话仍保留。', 'Task deleted. Run records and sessions are retained.'));
      await refresh();
    } catch (error) { throw new Error(String(error)); }
    finally { if (lifetime.current === life) setBusy(false); }
  }
  async function changeEnabled() {
    if (!selected || busy || dirty || missing) return;
    const life = lifetime.current, seat = view.current; setBusy(true);
    const mutation: ScheduleMutation = { action: 'update', task: { ...selected.task, enabled: !selected.task.enabled }, expected: selected.revision };
    try {
      const result = await changeSchedule(requestId(mutation), mutation);
      if (!alive(life, seat)) return;
      if (result.entry) { setSelected(result.entry); setDraft(structuredClone(result.entry.task)); setEntries(rows => rows.map(row => row.task.id === selected.task.id ? result.entry! : row)); setNotice(result.entry.registration_error || text('计划已更新', 'Schedule updated')); }
      await refresh();
    } catch (error) { if (alive(life, seat)) { setNotice(String(error)); await refresh(); } }
    finally { if (alive(life, seat)) setBusy(false); }
  }
  async function launch() {
    if (!selected || busy || dirty || missing) return;
    const life = lifetime.current, seat = view.current; setBusy(true);
    if (runRequest.current?.taskId !== selected.task.id || runRequest.current.revision !== selected.revision) runRequest.current = { taskId: selected.task.id, revision: selected.revision, id: randomId() };
    try {
      const result = await runSchedule(selected.task.id, runRequest.current.id, selected.revision);
      if (!alive(life, seat)) return;
      if (result.state === 'accepted' || result.state === 'error') runRequest.current = null;
      setNotice(result.error || (result.state === 'accepted' ? text('运行请求已受理，可在运行记录中查看进度。', 'Run admitted. Check Runs for progress.') : text('无法确认运行结果，请先查看运行记录。', 'The run outcome is uncertain. Check run history first.'))); await refresh();
    } catch (error) { if (alive(life, seat)) setNotice(String(error)); }
    finally { if (alive(life, seat)) setBusy(false); }
  }
  async function repair() {
    if (!selected || busy || dirty || missing) return;
    const life = lifetime.current, seat = view.current; setBusy(true);
    try { const row = await reconcileSchedule(selected.task.id); if (alive(life, seat)) { setSelected(row); setDraft(structuredClone(row.task)); setNotice(row.registration_error || text('已检查系统调度', 'Registration checked')); await refresh(); } }
    catch (error) { if (alive(life, seat)) setNotice(String(error)); }
    finally { if (alive(life, seat)) setBusy(false); }
  }
  async function moreHistory() {
    if (!selected || historyCursor === null || historyLoading) return;
    const controller = new AbortController(), life = lifetime.current, seat = view.current;
    historyController.current?.abort(); historyController.current = controller; setHistoryLoading(true); setHistoryError('');
    try { const page = await scheduleHistory(selected.task.id, controller.signal, historyCursor); if (alive(life, seat) && !controller.signal.aborted) { setHistory(old => [...page.events, ...old]); setHistoryCursor(page.next_cursor); } }
    catch (error) { if (alive(life, seat) && !controller.signal.aborted) setHistoryError(String(error)); }
    finally { if (alive(life, seat) && !controller.signal.aborted) setHistoryLoading(false); }
  }
  async function openRunSession(id: string) {
    const life = lifetime.current, seat = view.current; setBusy(true);
    try { await onSession(id, () => alive(life, seat)); }
    catch (error) { if (alive(life, seat)) setNotice(String(error)); }
    finally { if (alive(life, seat)) setBusy(false); }
  }
  async function createInChat() {
    const life = lifetime.current; setBusy(true); setNotice('');
    try { await onCreateInChat?.(); }
    catch (error) { if (lifetime.current === life) setNotice(String(error)); }
    finally { if (lifetime.current === life) setBusy(false); }
  }
  const statusText = (status: string) => ({ ok: text('已完成', 'Completed'), error: text('失败', 'Failed'), running: text('运行中', 'Running'), cancelled: text('已取消', 'Cancelled'), interrupted: text('已中断', 'Interrupted'), rejected: text('未执行', 'Rejected') }[status] || status);
  const weekday = (n: number) => lang === 'zh' ? `周${['一', '二', '三', '四', '五', '六', '日'][n - 1] || n}` : ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday'][n - 1] || String(n);
  const ruleText = (r: Rule) => r.kind === 'cron' ? text('旧 Cron 规则（暂不支持）', 'Legacy cron (unsupported)') : r.kind === 'daily' ? `${text('每天', 'Daily')} ${r.time}` : r.kind === 'weekly' ? `${weekday(r.weekday)} ${r.time}` : r.kind === 'interval' ? `${text('每', 'Every ')}${r.every_minutes}${text(' 分钟', ' min')}` : text('每小时', 'Hourly');
  const date = (at: number) => new Date(at * 1000).toLocaleString(lang === 'zh' ? 'zh-CN' : 'en', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  const problemText = (p: FormProblem) => ({ title: text('请输入 1–120 字的名称。', 'Enter a name of 1–120 characters.'), prompt: text('请输入任务指令，最多 64 KiB。', 'Enter instructions, at most 64 KiB.'), time: text('请选择有效的执行时间。', 'Choose a valid time.'), weekday: text('请选择有效的星期。', 'Choose a valid weekday.'), interval: text('间隔须为至少 1 分钟的整数。', 'The interval must be a whole number of at least 1 minute.'), cron: text('请选择当前系统支持的重复规则。', 'Choose a repeat rule supported by this host.') }[p]);

  function editor() {
    if (!draft) return null;
    return <section class="automation-editor" aria-label={text('任务详情', 'Task details')}>
      <div class="automation-editor-heading" tabIndex={-1}><span>{selected ? text('任务详情', 'Task details') : text('手动配置', 'Manual setup')}</span><button class="ghost-btn" disabled={busy} onClick={() => navigate(closeEditor)} aria-label={text('收起任务详情', 'Close task details')}>×</button></div>
      <div class="automation-tabs" role="tablist" aria-label={text('任务视图', 'Task views')} onKeyDown={event => {
        if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key) || !selected) return;
        event.preventDefault(); const next = event.key === 'Home' ? 'rule' : event.key === 'End' ? 'history' : tab === 'rule' ? 'history' : 'rule'; setTab(next);
        dialog.current?.querySelector<HTMLButtonElement>(`[data-task-tab="${next}"]`)?.focus();
      }}>
        <button id="automation-rule-tab" role="tab" type="button" data-task-tab="rule" aria-controls="automation-rule" aria-selected={tab === 'rule'} tabIndex={tab === 'rule' ? 0 : -1} onClick={() => setTab('rule')}>{text('规则', 'Rule')}</button>
        <button id="automation-history-tab" role="tab" type="button" data-task-tab="history" aria-controls="automation-history" disabled={!selected} aria-selected={tab === 'history'} tabIndex={tab === 'history' ? 0 : -1} onClick={() => setTab('history')}>{text('运行记录', 'Runs')}</button>
      </div>
      {missing && <div class="modal-error" role="alert">{text('任务已不存在。未保存的指令仍可复制；返回列表查看其他任务。', 'This task no longer exists. You can still copy your draft instructions or return to the list.')}</div>}
      {conflicting && <div class="automation-feedback" role="alert"><p>{text('同一字段已在其他位置修改，草稿已保留。请先核对最新版本。', 'The same field changed elsewhere. Your draft is preserved; review the latest version first.')}</p><div class="automation-actions"><button class="btn" disabled={busy} onClick={() => { if (latest && rebased) { setSelected(latest); setDraft(rebased.draft); } }}>{text('基于最新版本继续编辑', 'Continue on latest version')}</button><button class="btn" disabled={busy} onClick={() => navigate(() => { if (latest) open(latest); })}>{text('载入最新规则', 'Load latest rule')}</button></div></div>}
      {tab === 'rule' ? <form id="automation-rule" role="tabpanel" aria-labelledby="automation-rule-tab" onSubmit={event => { event.preventDefault(); void save(); }}>
        <fieldset disabled={busy || deletedId === selected?.task.id}>
          <div class="automation-section-title">{text('任务内容', 'Task')}</div>
          <label class="field-group"><span class="modal-label">{text('名称', 'Name')}</span><input class="menu-input" readOnly={missing} required maxLength={240} value={draft.title} onInput={event => edit({ title: event.currentTarget.value })} /></label>
          <label class="field-group"><span class="modal-label">{text('任务指令', 'Instructions')}</span><textarea class="menu-input" readOnly={missing} required rows={4} maxLength={65536} value={draft.prompt} onInput={event => edit({ prompt: event.currentTarget.value })} /></label>
          <div class="automation-section-title">{text('运行计划', 'Schedule')}</div>
          <div class="automation-field-row"><label class="field-group"><span class="modal-label">{text('重复', 'Repeat')}</span><select class="menu-input" disabled={missing} value={draft.schedule.kind} onChange={event => edit({ schedule: selectRule(draft.schedule, event.currentTarget.value) })}>
            {draft.schedule.kind === 'cron' && <option value="cron" disabled>{text('旧 Cron 规则', 'Legacy cron')}</option>}
            <option value="daily">{text('每天', 'Daily')}</option><option value="weekly">{text('每周', 'Weekly')}</option><option value="hourly">{text('每小时', 'Hourly')}</option><option value="interval">{text('固定间隔', 'Interval')}</option>
          </select></label>
          {(draft.schedule.kind === 'daily' || draft.schedule.kind === 'weekly') && <label class="field-group"><span class="modal-label">{text('时间', 'Time')}</span><input class="menu-input" readOnly={missing} type="time" required value={clockInputValue(draft.schedule.time)} onInput={event => edit({ schedule: { ...draft.schedule, time: event.currentTarget.value } as Rule })} /></label>}
          {draft.schedule.kind === 'weekly' && <label class="field-group"><span class="modal-label">{text('星期', 'Weekday')}</span><select class="menu-input" disabled={missing} value={draft.schedule.weekday} onChange={event => edit({ schedule: { ...draft.schedule, weekday: Number(event.currentTarget.value) } as Rule })}>{[1, 2, 3, 4, 5, 6, 7].map(n => <option key={n} value={n}>{weekday(n)}</option>)}</select></label>}
          {draft.schedule.kind === 'interval' && <label class="field-group"><span class="modal-label">{text('间隔（分钟）', 'Interval (minutes)')}</span><input class="menu-input" readOnly={missing} type="number" min="1" step="1" required value={draft.schedule.every_minutes} onInput={event => edit({ schedule: { kind: 'interval', every_minutes: Number(event.currentTarget.value) } })} /></label>}
          </div>
          <p class="field-hint automation-time-hint">{text('按执行主机本地时区运行', 'Runs in the execution host’s local time zone')}{current?.next_run && <> · {current.next_run.time_zone.replace('host local', text('主机本地', 'host local'))}</>}</p>
          <div class="automation-section-title">{text('执行设置', 'Execution')}</div>
          <label class="field-group"><span class="modal-label">{text('权限', 'Permissions')}</span><select class="menu-input" disabled={missing} value={draft.permission_mode} onChange={event => edit({ permission_mode: event.currentTarget.value })}><option value="plan">{text('只读分析', 'Read-only analysis')}</option><option value="accept_edits">{text('允许编辑；高风险操作仍拒绝', 'Allow edits; risky actions remain denied')}</option>{draft.permission_mode === 'auto' && <option value="auto">{text('兼容自动模式（按允许编辑执行）', 'Legacy auto (runs with edit permissions)')}</option>}</select></label>
          <div class="automation-context"><span class="modal-label">{text('工作目录', 'Working directory')}</span><code>{draft.cwd}</code><p class="field-hint">{text('使用执行主机当前默认模型，每次在独立会话中运行。', 'Uses the host’s default model in a fresh session for each run.')}</p></div>
          {current?.registration_error && <div class="modal-error" role="alert">{current.registration_error}</div>}
          {current?.next_run && <p class="field-hint">{text('预计下次（本设备时间）', 'Estimated next (device time)')}: {date(current.next_run.at)}</p>}
          {(dirty || !selected) && <div class="automation-savebar">{problem && dirty && <p class="modal-error" role="alert">{problemText(problem)}</p>}<span class="field-hint">{selected ? text('有未保存的更改', 'Unsaved changes') : text('确认后启用系统调度', 'Saving enables OS scheduling')}</span><div class="automation-actions"><button class="btn" type="button" onClick={() => { if (selected) setDraft(structuredClone(selected.task)); else closeEditor(); }}>{text('取消', 'Cancel')}</button><button class="btn btn-primary" type="submit" disabled={Boolean(problem) || conflicting || missing}>{busy ? text('保存中…', 'Saving…') : selected ? text('保存更改', 'Save changes') : text('创建任务', 'Create task')}</button></div></div>}
        </fieldset>
        {selected && <div class="automation-task-actions"><button class="btn" type="button" disabled={busy || dirty || missing || Boolean(deletedId)} onClick={() => void changeEnabled()}>{selected.task.enabled ? text('暂停计划', 'Pause schedule') : text('启用计划', 'Enable schedule')}</button><button class="btn" type="button" disabled={busy || dirty || missing || !selected.task.enabled || current?.running || Boolean(deletedId)} onClick={() => void launch()}>{text('立即运行', 'Run now')}</button>{current?.registration !== 'registered' && <button class="btn" type="button" disabled={busy || dirty || missing || Boolean(deletedId)} onClick={() => void repair()}>{text('修复调度', 'Repair registration')}</button>}<button class="ghost-btn automation-danger" type="button" disabled={busy || missing || Boolean(deletedId)} onClick={() => setDeleteConfirm(selected)}>{text('删除任务', 'Delete task')}</button></div>}
      </form> : <section id="automation-history" class="automation-history" role="tabpanel" aria-labelledby="automation-history-tab">
        <p class="field-hint">{text('这里记录实际执行结果。删除或修改计划不会改写当次保存的指令。', 'These are execution results. Editing or deleting a schedule does not rewrite its saved instructions.')}</p>
        {historyError && <div class="modal-error" role="alert">{historyError}<button class="btn" onClick={() => setHistoryReload(value => value + 1)}>{text('重新加载记录', 'Reload runs')}</button></div>}
        {historyLoading && <p class="field-hint" role="status">{text('正在加载运行记录…', 'Loading runs…')}</p>}
        {!historyError && !historyLoading && history.length === 0 && <div class="automation-empty"><ScheduleIcon/><p>{text('还没有运行记录', 'No runs yet')}</p><span class="field-hint">{text('任务执行后，可在这里打开对应会话。', 'After a run, open its session here.')}</span></div>}
        {foldScheduleRuns(history).map(run => <article key={run.id} class="automation-run"><div class="automation-run-summary"><ScheduleIcon/><time>{run.startedAt === undefined ? text('开始时间未记录', 'Start not recorded') : date(run.startedAt)}</time><span class={'automation-badge status-' + run.status}>{statusText(run.status)}</span>{run.sessionId && <button class="btn" disabled={busy} onClick={() => navigate(() => void openRunSession(run.sessionId!))}>{text('打开会话', 'Open session')}</button>}</div>{run.prompt ? <details><summary>{run.prompt.split('\n')[0]}</summary><p>{run.prompt}</p></details> : <p class="field-hint">{text('本页未包含保存的指令，可加载更早的记录。', 'This page has no saved instruction. Load earlier records to find it.')}</p>}{run.error && <p class="automation-danger">{run.error}</p>}</article>)}
        {historyCursor !== null && <button class="btn" disabled={historyLoading} onClick={() => void moreHistory()}>{text('加载更早的记录', 'Load earlier runs')}</button>}
      </section>}
    </section>;
  }

  return <>
    <div class="modal-overlay automation-overlay" onClick={event => { if (event.target === event.currentTarget) navigate(onClose); }}>
      <div class="modal-card automation-modal" role="dialog" aria-modal="true" aria-labelledby="automation-title" ref={dialog} onKeyDown={event => {
        if (deleteConfirm || discardAction || event.defaultPrevented) return;
        if (event.key === 'Escape') { event.stopPropagation(); event.preventDefault(); navigate(draft ? closeEditor : onClose); }
        if (event.key === 'Tab') {
          const nodes = Array.from(dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),select:not(:disabled),a[href]') || []).filter(node => node.getClientRects().length);
          const first = nodes[0], last = nodes[nodes.length - 1];
          if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
          else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
        }
      }}>
        <div class="modal-header"><ScheduleIcon/><h3 id="automation-title">{text('自动化任务', 'Automations')}</h3><span class="modal-sub">{entries.length}</span><button class="ghost-btn modal-close" onClick={() => navigate(onClose)} aria-label={text('关闭', 'Close')}>×</button></div>
        <div class="modal-body automation-body">
          <div class="automation-intro"><div><h4>{text('让重复的工作按计划完成', 'Put recurring work on a schedule')}</h4><p class="field-hint">{text('在对话中描述任务，或手动配置；每次运行都有独立会话。', 'Describe a task in chat or configure it manually. Each run has its own session.')}</p></div><div class="automation-actions">{onCreateInChat && <button class="btn btn-primary" disabled={busy} onClick={() => navigate(() => { closeEditor(); void createInChat(); })}>＋ {text('新建任务', 'New task')}</button>}<button class={'btn' + (onCreateInChat ? '' : ' btn-primary')} disabled={busy} onClick={() => navigate(() => open(null))}>{text('手动配置', 'Manual setup')}</button></div></div>
          {notice && <div class="automation-feedback" role="status">{notice}</div>}
          <div class="automation-toolbar"><div class="segmented" role="group" aria-label={text('状态筛选', 'Filter status')}>{[['all', text('全部', 'All')], ['enabled', text('已启用', 'Enabled')], ['disabled', text('已暂停', 'Paused')]].map(([value, label]) => <button key={value} class={'segmented-btn' + (filter === value ? ' active' : '')} aria-pressed={filter === value} onClick={() => setFilter(value)}>{label}</button>)}</div><input class="menu-input automation-search" type="search" aria-label={text('搜索任务', 'Search tasks')} placeholder={text('搜索名称、指令或工作目录', 'Search name, instructions or directory')} value={query} onInput={event => setQuery(event.currentTarget.value)}/><button class="ghost-btn" onClick={() => void refresh()} aria-label={text('刷新任务列表', 'Refresh tasks')}>{loading && settled ? text('刷新中…', 'Refreshing…') : text('刷新', 'Refresh')}</button></div>
          {loadError && <div class="modal-error" role="alert">{loadError} <button class="btn" onClick={() => void refresh()}>{text('重试加载任务', 'Retry loading tasks')}</button></div>}
          {!settled && loading && <div class="modal-loading">{text('正在加载任务…', 'Loading tasks…')}</div>}
          {!selected && draft && <div class="automation-card is-open">{editor()}</div>}
          <div class="automation-catalog">{rows.map(entry => <article key={entry.task.id} class={'automation-card' + (selected?.task.id === entry.task.id ? ' is-open' : '')}>
            <button class="automation-card-heading" disabled={busy} aria-expanded={selected?.task.id === entry.task.id} onClick={() => navigate(() => selected?.task.id === entry.task.id ? closeEditor() : open(entry))}><ScheduleIcon/><div class="automation-card-content"><strong>{entry.task.title}</strong><span>{ruleText(entry.task.schedule)} · {entry.task.cwd}</span>{entry.next_run && entry.task.enabled && !(missing && selected?.task.id === entry.task.id) && <small>{text('预计下次（本设备）', 'Next (device time)')}: {date(entry.next_run.at)}</small>}</div><span class="automation-badge">{missing && selected?.task.id === entry.task.id ? text('已删除', 'Deleted') : entry.task.enabled ? text('已启用', 'Enabled') : text('已暂停', 'Paused')}</span>{entry.running && !(missing && selected?.task.id === entry.task.id) && <span class="automation-badge status-running">{text('运行中', 'Running')}</span>}{entry.task.enabled && entry.registration !== 'registered' && !(missing && selected?.task.id === entry.task.id) && <span class="automation-registration-warning" title={entry.registration_error || text('系统调度需检查', 'Check OS registration')}>!</span>}<span class="automation-chevron" aria-hidden="true">{selected?.task.id === entry.task.id ? '−' : '+'}</span></button>
            {selected?.task.id === entry.task.id && editor()}
          </article>)}</div>
          {settled && !loadError && visible.length === 0 && !selected && <div class="automation-empty"><ScheduleIcon/><p>{entries.length === 0 ? text('还没有自动化任务', 'No automations yet') : text('没有匹配的任务', 'No matching tasks')}</p><span class="field-hint">{entries.length === 0 ? text('新建任务后，可在这里查看规则和运行记录。', 'Create a task to manage its rule and runs here.') : text('试试其他关键词或状态筛选。', 'Try another search or status filter.')}</span></div>}
        </div>
      </div>
    </div>
    {deleteConfirm && <ConfirmDialog title={text('删除自动化任务', 'Delete automation')} body={text(`确定删除「${deleteConfirm.task.title}」？未来触发将停止，已启动的运行、记录和会话仍保留。`, `Delete “${deleteConfirm.task.title}”? Future triggers stop; existing runs, records and sessions are retained.`)} confirmLabel={text('删除任务', 'Delete task')} cancelLabel={text('取消', 'Cancel')} onConfirm={remove} onClose={() => { if (!busy) setDeleteConfirm(null); }}/>}
    {discardAction && <ConfirmDialog title={text('放弃未保存的更改？', 'Discard unsaved changes?')} body={text('当前草稿尚未保存，离开后将丢弃这些更改。', 'This draft has not been saved. Leaving will discard your changes.')} danger={false} icon="" confirmLabel={text('放弃更改', 'Discard changes')} cancelLabel={text('继续编辑', 'Keep editing')} onConfirm={() => { const action = discardAction; setDiscardAction(null); action(); }} onClose={() => setDiscardAction(null)}/>}
  </>;
}
