import { useEffect, useRef, useState } from 'preact/hooks';
import { useSettings } from '../settings';
import { randomId } from '../lib/randomId';
import { watchSchedules, changeSchedule, listSchedules, reconcileSchedule, runSchedule, scheduleHistory, type Rule, type RunEvent, type ScheduleEntry, type ScheduledTask, type ScheduleMutation } from '../lib/scheduleApi';
import './Automation.css';
import { foldScheduleRuns } from '../lib/scheduleHistory';

export function Automation({ cwd, onClose, onSession }: { cwd: string; onClose: () => void; onSession: (id: string) => Promise<void> }) {
  const { lang } = useSettings();
  const text = (zh: string, en: string) => lang === 'zh' ? zh : en;
  const [entries,setEntries] = useState<ScheduleEntry[]>([]);
  const [loading,setLoading] = useState(true);
  const [error,setError] = useState('');
  const [query,setQuery] = useState('');
  const [filter,setFilter] = useState('all');
  const [selected,setSelected] = useState<ScheduleEntry | null>(null);
  const [draft,setDraft] = useState<ScheduledTask | null>(null);
  const [history,setHistory] = useState<RunEvent[]>([]);
  const [historyError,setHistoryError] = useState('');
  const [historyCursor,setHistoryCursor] = useState<number|null>(null);
  const historyController = useRef<AbortController|null>(null);
  const [historyLoading,setHistoryLoading] = useState(false);
  const [historyReload,setHistoryReload] = useState(0);
  const [tab,setTab] = useState<'rule'|'history'>('rule');
  const [busy,setBusy] = useState(false);
  const [notice,setNotice] = useState('');
  const [confirmDelete,setConfirmDelete] = useState(false);
  const [confirmReload,setConfirmReload] = useState(false);
  const life = useRef(0);
  const readSequence = useRef(0);
  const dialog = useRef<HTMLDivElement>(null);
  const operation = useRef<{signature:string;id:string}|null>(null);
  const runRequest = useRef<{taskId:string;id:string}|null>(null);
  const fresh = () => ({ id: randomId(), title: '', prompt: '', cwd, schedule: {kind:'daily',time:'09:00'} as Rule, permission_mode:'plan', notify:'important',enabled:true,created_at:0 });
  async function refresh(signal?: AbortSignal) {
    const generation = life.current;
    const read = ++readSequence.current;
    try { const rows = await listSchedules(signal); if (generation === life.current && read === readSequence.current && !signal?.aborted) { setEntries(rows); setError(''); } }
    catch (e) { if (generation === life.current && read === readSequence.current && !signal?.aborted) setError(String(e)); }
    finally { if (generation === life.current && read === readSequence.current && !signal?.aborted) setLoading(false); }
  }
  useEffect(() => {
    const controller = new AbortController();
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.querySelector<HTMLButtonElement>('button')?.focus();
    void refresh(controller.signal);
    void watchSchedules(() => { if (document.visibilityState !== 'hidden') void refresh(controller.signal); },controller.signal).catch(() => { /* Polling and foreground reads recover a lost subscription. */ });
    const update = () => { if (document.visibilityState !== 'hidden') void refresh(controller.signal); };
    const timer = window.setInterval(update,10000);
    window.addEventListener('online',update); document.addEventListener('visibilitychange',update);
    return () => { life.current++; controller.abort(); historyController.current?.abort(); clearInterval(timer); window.removeEventListener('online',update); document.removeEventListener('visibilitychange',update); previous?.focus(); };
  },[]);
  useEffect(() => {
    if (tab !== 'history' || !selected) return;
    const controller = new AbortController();
    historyController.current?.abort(); historyController.current = controller;
    setHistory([]); setHistoryError(''); setHistoryLoading(true);
    scheduleHistory(selected.task.id,controller.signal).then(rows => { if (!controller.signal.aborted) {setHistory(rows.events);setHistoryCursor(rows.next_cursor);} }).catch(e => { if (!controller.signal.aborted) setHistoryError(String(e)); }).finally(() => { if (!controller.signal.aborted) setHistoryLoading(false); });
    return () => controller.abort();
  },[tab,selected?.task.id,JSON.stringify(entries.find(e=>e.task.id===selected?.task.id)?.last_run),historyReload]);
  async function moreHistory() {
    if (!selected || historyCursor === null || historyLoading) return;
    const controller = new AbortController(); historyController.current?.abort(); historyController.current=controller;
    setHistoryLoading(true); setHistoryError('');
    try {const page=await scheduleHistory(selected.task.id,controller.signal,historyCursor);if(!controller.signal.aborted){setHistory(old=>[...page.events,...old]);setHistoryCursor(page.next_cursor);}}
    catch(e){if(!controller.signal.aborted)setHistoryError(String(e));}
    finally{if(!controller.signal.aborted)setHistoryLoading(false);}
  }
  function select(entry: ScheduleEntry | null) {
    historyController.current?.abort(); setHistory([]); setHistoryCursor(null); setHistoryError(''); setHistoryLoading(false);
    setSelected(entry); setDraft(entry ? structuredClone(entry.task) : fresh());
    setTab('rule'); setConfirmDelete(false); setConfirmReload(false); setNotice(''); operation.current = null; runRequest.current = null;
  }
  function edit(patch: Partial<ScheduledTask>) { if (draft) setDraft({...draft,...patch}); }
  async function saveMutation(mutation: ScheduleMutation) {
    const signature = JSON.stringify(mutation);
    if (operation.current?.signature !== signature) operation.current = {signature,id:randomId()};
    const generation = life.current;
    setBusy(true); setNotice('');
    try {
      const result = await changeSchedule(operation.current.id,mutation);
      if (generation !== life.current) return;
      if (result.entry) { setSelected(result.entry); setDraft(structuredClone(result.entry.task)); setNotice(result.entry.registration_error || text('已保存','Saved')); }
      else { setSelected(null); setDraft(null); setNotice(result.warning || text('已删除；已启动的运行不会因此取消。','Deleted; runs already started are not cancelled.')); }
      setConfirmDelete(false); await refresh();
    } catch(e) { if (generation === life.current) {setNotice(`${String(e)} · ${text('请刷新核对状态，重试同一修改不会重复创建。','Refresh to check state; retrying the same change will not create a duplicate.')}`);await refresh();} }
    finally { if (generation === life.current) setBusy(false); }
  }
  async function launch() {
    if (!selected) return;
    if (runRequest.current?.taskId !== selected.task.id) runRequest.current = {taskId:selected.task.id,id:randomId()};
    const generation = life.current; setBusy(true);
    try { const result = await runSchedule(selected.task.id,runRequest.current.id,selected.revision); if (generation === life.current) { if (!result.error) runRequest.current = null; setNotice(result.error || text('运行请求已受理，请在运行记录中查看结果。','Run admitted. Check run history for its result.')); await refresh(); } }
    catch(e) { if (generation === life.current) setNotice(String(e)); }
    finally { if (generation === life.current) setBusy(false); }
  }
  const dirty = Boolean(selected && draft && JSON.stringify(selected.task) !== JSON.stringify(draft));
  const latest = entries.find(entry => entry.task.id === selected?.task.id);
  const visible = entries.filter(e => (filter === 'all' || e.task.enabled === (filter === 'enabled')) && `${e.task.title} ${e.task.prompt} ${e.task.cwd}`.toLowerCase().includes(query.toLowerCase()));
  const statusText = (status: string) => ({ok:text('已完成','Completed'),error:text('失败','Failed'),running:text('运行中','Running'),cancelled:text('已取消','Cancelled'),interrupted:text('已中断','Interrupted'),rejected:text('未执行','Rejected')}[status] || status);
  const rule = (r: Rule) => r.kind === 'cron' ? `${text('旧 Cron 规则（暂不支持）','Legacy cron (unsupported)')} ${r.expr}` : r.kind === 'daily' ? `${text('每天','Daily')} ${r.time}` : r.kind === 'weekly' ? `${text('每周','Weekly')} ${r.weekday} · ${r.time}` : r.kind === 'interval' ? `${text('每','Every')} ${r.every_minutes} ${text('分钟','min')}` : text('每小时','Hourly');
  return <div class="automation-overlay" role="dialog" aria-modal="true" aria-label={text('自动化任务','Automations')} ref={dialog} onKeyDown={e => {
    if (e.key === 'Escape') { e.stopPropagation(); onClose(); }
    if (e.key === 'Tab') {
      const nodes = Array.from(dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled),input,textarea,select,a[href]') || []).filter(el => el.getClientRects().length);
      const first = nodes[0], last = nodes[nodes.length-1];
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last?.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first?.focus(); }
    }
  }}>
    <header class="automation-header"><div><h1>{text('自动化任务','Automations')}</h1><p>{text('按计划运行，在独立会话中保留结果','Scheduled work, with results kept in separate sessions')}</p></div><button onClick={onClose}>{text('关闭','Close')}</button></header>
    <div class="automation-layout">
      <aside class={'automation-list' + (draft ? ' has-selection' : '')}>
        <div class="automation-list-tools"><button disabled={busy} onClick={() => select(null)}>＋ {text('新建任务','New task')}</button><button onClick={() => void refresh()}>{text('刷新','Refresh')}</button></div>
        <input aria-label={text('搜索任务','Search tasks')} placeholder={text('搜索任务或工作目录','Search tasks or directory')} value={query} onInput={e => setQuery(e.currentTarget.value)} />
        <select aria-label={text('状态筛选','Filter status')} value={filter} onChange={e => setFilter(e.currentTarget.value)}><option value="all">{text('全部任务','All tasks')}</option><option value="enabled">{text('已启用','Enabled')}</option><option value="disabled">{text('已暂停','Paused')}</option></select>
        {loading && <p role="status">{text('加载中…','Loading…')}</p>}
        {error && <p role="alert">{error}</p>}
        {!loading && !error && visible.length === 0 && <p>{text('暂无匹配的任务','No matching tasks')}</p>}
        {visible.map(entry => <button key={entry.task.id} class={'automation-task' + (selected?.task.id === entry.task.id ? ' selected' : '')} disabled={busy} onClick={() => select(entry)}>
          <strong>{entry.task.title}</strong><span>{rule(entry.task.schedule)} · {entry.task.enabled ? text('已启用','Enabled') : text('已暂停','Paused')}</span><small>{entry.task.cwd}</small>
          <span>{entry.running ? text('运行中','Running') : entry.registration === 'registered' ? text('系统调度已注册','Registered') : text('调度需检查','Check registration')}</span>
        </button>)}
      </aside>
      <main class="automation-detail">
        {!draft ? <div class="automation-empty"><h2>{text('把重复的工作交给计划','Put recurring work on a schedule')}</h2><p>{text('选择任务查看规则与运行记录，或新建一个任务。','Select a task to inspect its rule and runs, or create one.')}</p><button onClick={() => select(null)}>{text('新建任务','Create task')}</button></div> : <>
          <div class="automation-detail-heading"><button disabled={busy} onClick={() => {setDraft(null);setSelected(null);}}>← {text('任务列表','Tasks')}</button><h2>{selected ? selected.task.title : text('新建任务','New task')}</h2></div>
          <div class="automation-tabs" role="tablist"><button role="tab" aria-selected={tab === 'rule'} onClick={() => setTab('rule')}>{text('规则','Rule')}</button><button role="tab" disabled={!selected} aria-selected={tab === 'history'} onClick={() => setTab('history')}>{text('运行记录','Runs')}</button></div>
          {notice && <p class="automation-notice" role="status">{notice}</p>}
          {selected && latest && selected.revision !== latest.revision && <div class="automation-notice"><p>{text('任务已在其他位置更新，当前草稿已保留。','This task changed elsewhere. Your draft is preserved.')}</p><button disabled={busy} onClick={() => dirty ? setConfirmReload(true) : select(latest)}>{text('载入最新规则','Load latest rule')}</button>{confirmReload && <p>{text('载入将丢弃当前未保存草稿。','Loading discards this unsaved draft.')} <button onClick={() => select(latest)}>{text('确认载入','Confirm reload')}</button><button onClick={() => setConfirmReload(false)}>{text('取消','Cancel')}</button></p>}</div>}
          {tab === 'rule' ? <form onSubmit={e => { e.preventDefault(); void saveMutation(selected ? {action:'update',task:draft,expected:selected.revision} : {action:'create',task:draft}); }}>
            <fieldset disabled={busy}>
              <label>{text('名称','Name')}<input required maxLength={120} value={draft.title} onInput={e => edit({title:e.currentTarget.value})} /></label>
              <label>{text('任务指令','Instructions')}<textarea required rows={5} maxLength={65536} value={draft.prompt} onInput={e => edit({prompt:e.currentTarget.value})} /></label>
              <label>{text('工作目录','Working directory')}<input required value={draft.cwd} onInput={e => edit({cwd:e.currentTarget.value})} /></label>
              <label>{text('重复规则','Repeat')}<select value={draft.schedule.kind} onChange={e => { const k = e.currentTarget.value; edit({schedule:k === 'daily' ? {kind:'daily',time:'09:00'} : k === 'weekly' ? {kind:'weekly',weekday:1,time:'09:00'} : k === 'hourly' ? {kind:'hourly'} : {kind:'interval',every_minutes:60}}); }}>{draft.schedule.kind === 'cron' && <option value="cron" disabled>{text('旧 Cron 规则：请选择受支持规则','Legacy cron: select a supported rule')}</option>}<option value="daily">{text('每天','Daily')}</option><option value="weekly">{text('每周','Weekly')}</option><option value="hourly">{text('每小时','Hourly')}</option><option value="interval">{text('固定间隔','Interval')}</option></select></label>
              {(draft.schedule.kind === 'daily' || draft.schedule.kind === 'weekly') && <label>{text('执行时间（执行主机本地时区）','Time (execution host local zone)')}<input type="time" required value={draft.schedule.time} onInput={e => edit({schedule:{...draft.schedule,time:e.currentTarget.value} as Rule})} /></label>}
              {draft.schedule.kind === 'weekly' && <label>{text('星期','Weekday')}<select value={draft.schedule.weekday} onChange={e => edit({schedule:{...draft.schedule,weekday:Number(e.currentTarget.value)} as Rule})}>{[1,2,3,4,5,6,7].map(n => <option value={n}>{text('周','Day ')}{n}</option>)}</select></label>}
              {draft.schedule.kind === 'interval' && <label>{text('间隔分钟数','Interval in minutes')}<input type="number" min="1" required value={draft.schedule.every_minutes} onInput={e => edit({schedule:{kind:'interval',every_minutes:Number(e.currentTarget.value)}})} /></label>}
              <label>{text('权限','Permissions')}<select value={draft.permission_mode} onChange={e => edit({permission_mode:e.currentTarget.value})}><option value="plan">{text('只读分析','Read-only analysis')}</option><option value="accept_edits">{text('允许编辑；高风险操作仍拒绝','Allow edits; risky actions remain denied')}</option>{draft.permission_mode === 'auto' && <option value="auto">{text('兼容自动模式（按允许编辑执行）','Legacy auto (runs with edit permissions)')}</option>}</select></label>
              <p class="automation-hint">{text('使用执行主机当前默认模型。关闭浏览器不影响系统调度；主机仍需可用。','Uses the host’s current default model. Closing the browser does not stop OS scheduling; the host must remain available.')}</p>
              {selected?.next_run && <p>{text('预计下次（本设备时间）','Estimated next (device time)')}: {new Date(selected.next_run.at*1000).toLocaleString()} · {selected.next_run.time_zone}</p>}
              {selected?.registration_error && <p role="alert">{selected.registration_error}</p>}
              <div class="automation-buttons"><button type="submit">{selected ? text('保存更改','Save changes') : text('创建任务','Create task')}</button>{selected && <><button type="button" disabled={dirty} onClick={() => void saveMutation({action:'update',task:{...selected.task,enabled:!selected.task.enabled},expected:selected.revision})}>{selected.task.enabled ? text('暂停计划','Pause schedule') : text('启用计划','Enable schedule')}</button><button type="button" disabled={dirty || !selected.task.enabled || Boolean(latest?.running)} onClick={() => void launch()}>{text('立即运行','Run now')}</button><button type="button" disabled={dirty} onClick={async () => {const generation=life.current;setBusy(true);try{ const row=await reconcileSchedule(selected.task.id);if(generation!==life.current)return;setSelected(row);setNotice(row.registration_error || text('已检查调度','Registration checked'));await refresh();}catch(e){if(generation===life.current)setNotice(String(e));}finally{if(generation===life.current)setBusy(false);}}}>{text('修复调度','Repair registration')}</button><button type="button" class="automation-delete" onClick={() => setConfirmDelete(true)}>{text('删除','Delete')}</button></>}</div>
              {confirmDelete && selected && <div class="automation-delete-confirm"><p>{text('删除任务会停止未来触发，运行记录保留。确定删除？','Delete this task and stop future triggers? Run history will be retained.')}</p><button type="button" onClick={() => void saveMutation({action:'delete',id:selected.task.id,expected:selected.revision})}>{text('确认删除','Confirm delete')}</button><button type="button" onClick={() => setConfirmDelete(false)}>{text('取消','Cancel')}</button></div>}
            </fieldset>
          </form> : <section class="automation-history">
            {historyLoading && <p role="status">{text('正在加载运行记录…','Loading runs…')}</p>}
            {historyError && <p role="alert">{historyError} <button onClick={() => setHistoryReload(n=>n+1)}>{text('重试','Retry')}</button></p>}
            {!historyError && !historyLoading && history.length === 0 && <p>{text('暂无运行记录','No run records yet')}</p>}
            {foldScheduleRuns(history).map(run => <article key={run.id}><time>{run.startedAt === undefined ? text('开始时间未记录','Start not recorded') : new Date(run.startedAt*1000).toLocaleString()}</time><strong>{statusText(run.status)}</strong>{run.prompt ? <p>{run.prompt}</p> : <p class="automation-hint">{text('本页未包含保存的指令','Saved instruction is not included on this page')}</p>}{run.error && <p>{run.error}</p>}{run.sessionId && <button onClick={() => void onSession(run.sessionId!).catch(e => setNotice(String(e)))}>{text('打开会话','Open session')}</button>}</article>)}
            {historyCursor !== null && <button disabled={historyLoading} onClick={() => void moreHistory()}>{text('加载更早的记录','Load earlier runs')}</button>}
          </section>}
        </>}
      </main>
    </div>
  </div>;
}
