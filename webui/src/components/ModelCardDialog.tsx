import { useEffect, useRef, useState } from 'preact/hooks';
import { getModelCards, type ModelInfo } from '../api';
import { useT } from '../settings';
import type { MsgKey } from '../i18n';
import { safeSource, sameConditions, scoreLabel, taskComparable, type Evaluation, type ModelCard, type ModelCards } from '../lib/modelCards';
import './model-cards.css';

export function ModelCardDialog({ model, onClose }: { model: ModelInfo; onClose: () => void }) {
  const t = useT();
  const dialog = useRef<HTMLDialogElement>(null);
  const [data, setData] = useState<ModelCards | null>(null);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  const noPublicData = model.model_card_status === 'no_public_data';
  const [browse, setBrowse] = useState(!noPublicData);
  const [primary, setPrimary] = useState(model.model_card_id ?? '');
  const [secondary, setSecondary] = useState('');
  useEffect(() => {
    const element = dialog.current!;
    element.showModal();
    const closed = () => onClose();
    element.addEventListener('cancel', closed);
    return () => { element.removeEventListener('cancel', closed); element.close(); };
  }, []);
  useEffect(() => {
    if (!browse) return;
    let live = true;
    setFailed(false);
    getModelCards().then(value => { if (live) setData(value); }).catch(() => { if (live) setFailed(true); });
    return () => { live = false; };
  }, [retry, browse]);
  const a = data?.models[primary];
  const b = data?.models[secondary];
  const taskKeys: Record<string, MsgKey> = { agents: 'cards.task.agents', coding: 'cards.task.coding', chat: 'cards.task.chat', math: 'cards.task.math', vision: 'cards.task.vision', search: 'cards.task.search' };
  const taskLabel = (id: string) => taskKeys[id] ? t(taskKeys[id]) : id;
  const byLabel = (by: string) => t(`cards.by.${by}` as MsgKey);
  const link = (url: string | undefined, label: string) => {
    const href = safeSource(url);
    return href ? <a href={href} target="_blank" rel="noopener noreferrer">{label} ↗</a> : <span>{label}</span>;
  };
  const cell = (row: Evaluation | undefined, other?: Evaluation) => {
    if (!row || !data) return <span class="mc-muted">—</span>;
    const bench = data.benchmarks[row.bench];
    const bar = bench.kind === 'acc' && bench.raw_scale_max === undefined && row.score >= 0 && row.score <= 1
      && (!other || sameConditions(row, other));
    return <div class="mc-score"><strong>{scoreLabel(row.score, bench)}</strong>
      {bar && <span class="mc-bar" aria-hidden="true"><i style={{ width: `${row.score * 100}%` }} /></span>}
      <details><summary>{t('cards.conditions')}</summary><small>{byLabel(row.by)} · {row.date ?? '—'}<br />{row.note ?? '—'}</small></details>
    </div>;
  };
  const rows: { a?: Evaluation; b?: Evaluation }[] = [];
  if (a) {
    const remaining = [...(b?.evaluations ?? [])];
    for (const row of a.evaluations) {
      const at = remaining.findIndex(other => sameConditions(row, other));
      rows.push({ a: row, b: at >= 0 ? remaining.splice(at, 1)[0] : undefined });
    }
    for (const row of remaining) rows.push({ b: row });
  }
  const cardHeading = (card: ModelCard) => <><strong>{card.name}</strong><small>{card.maker}</small></>;
  return <dialog class="model-card-dialog" ref={dialog} aria-labelledby="model-card-title" onClick={e => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (e.target === e.currentTarget && (e.clientX < rect.left || e.clientX > rect.right || e.clientY < rect.top || e.clientY > rect.bottom)) onClose();
  }}>
    <header><div><h2 id="model-card-title">{t('cards.title')}</h2><p>{model.model}</p></div><button type="button" onClick={onClose} aria-label={t('cards.close')}>×</button></header>
    <div class="mc-body">
      <div class="mc-config"><span>{t('cards.context')}: {model.context_window?.toLocaleString() ?? '—'}</span><span>{t('cards.vision')}: {t(model.supports_vision == null ? 'cards.unknown' : model.supports_vision ? 'cards.yes' : 'cards.no')}</span></div>
      <p class="mc-muted">{t('cards.contextSource')}</p>
      {['auto', 'auto-beta'].includes(model.model.split('/').pop() ?? '') && <p class="mc-muted">{t('providers.contextBudgetHint')}</p>}
      {noPublicData && <p class="mc-empty mc-no-public" role="status">{t('cards.noPublic')}</p>}
      {!browse && <button type="button" class="mc-browse" onClick={() => setBrowse(true)}>{t('cards.browseCatalog')}</button>}
      {browse && !data && !failed && <p role="status">{t('cards.loading')}</p>}
      {failed && <p role="alert">{t('cards.failed')} <button type="button" onClick={() => setRetry(n => n + 1)}>{t('cards.retry')}</button></p>}
      {data && <>
        {!model.model_card_id && !noPublicData && <p class="mc-empty">{t('cards.empty')}</p>}
        <p class="mc-muted">{t('cards.reviewing')}</p>
        <div class="mc-pickers">
          <label>{t('cards.catalog')}<select value={primary} onChange={e => { setPrimary(e.currentTarget.value); if (e.currentTarget.value === secondary) setSecondary(''); }}><option value="">—</option>{Object.values(data.models).map(card => <option key={card.id} value={card.id}>{card.name}</option>)}</select></label>
          <label>{t('cards.compare')}<select value={secondary} disabled={!a} onChange={e => setSecondary(e.currentTarget.value)}><option value="">{t('cards.none')}</option>{Object.values(data.models).filter(card => card.id !== primary).map(card => <option key={card.id} value={card.id}>{card.name}</option>)}</select></label>
        </div>
        {a && <>
          <div class={'mc-headings' + (b ? ' comparing' : '')}>{[a, ...(b ? [b] : [])].map(card => <section key={card.id}>{cardHeading(card)}
            {card.best_task && <p>{t('cards.best')}: {taskLabel(card.best_task)}</p>}
            {card.worst_task && <p class="mc-muted">{t('cards.worst')}: {taskLabel(card.worst_task)}</p>}
            {card.valid_benchmark_count < 5 && <p class="mc-muted">{t('cards.sparse')}</p>}
          </section>)}</div>
          <h3>{t('cards.tasks')}</h3><p class="mc-muted">{t('cards.scope')}</p>
          <div class="mc-table-scroll"><table><thead><tr><th>{t('cards.tasks')}</th><th>{a.name}</th>{b && <th>{b.name}</th>}</tr></thead><tbody>
            {[...new Set([...a.tasks, ...(b?.tasks ?? [])].map(task => task.id))].map(id => {
              const left = a.tasks.find(task => task.id === id); const right = b?.tasks.find(task => task.id === id);
              return <tr key={id}><th>{taskLabel(id)}{left && right && !taskComparable(left, right) && <small>{t('cards.different')}</small>}</th>{[left, ...(b ? [right] : [])].map((task, index) => <td key={index}>{task?.g == null ? '—' : task.g.toFixed(1)}<small>{t('cards.rank')}: {task?.rank == null ? '—' : `${task.rank} / ${task.total}`}<br />{t('cards.evidence')}: {task?.n ?? 0}</small></td>)}</tr>;
            })}
          </tbody></table></div>
          <h3>{t('cards.benchmarks')}</h3>
          {b && rows.some(pair => !pair.a || !pair.b) && <p class="mc-muted">{t('cards.different')}</p>}
          <div class="mc-table-scroll"><table><thead><tr><th>Benchmark</th><th>{a.name}</th>{b && <th>{b.name}</th>}</tr></thead><tbody>{rows.map((pair, index) => <tr key={index}><th>{data.benchmarks[(pair.a ?? pair.b)!.bench].name}</th><td>{cell(pair.a, pair.b)}</td>{b && <td>{cell(pair.b, pair.a)}</td>}</tr>)}</tbody></table></div>
        </>}
        <details class="mc-notes"><summary>{t('cards.notes')}</summary>
          <p>{data.upstream.updated} · {data.upstream.commit.slice(0, 12)}</p><p>{data.attribution.credit}</p><p>{link(data.attribution.license_url, data.attribution.license)} · {link(data.attribution.upstream_notices, t('cards.notes'))}</p>
          <pre>{data.attribution.notice}</pre>
          {[a, b].filter((card): card is ModelCard => !!card).map(card => <section key={card.id}><h4>{card.name}</h4>{card.identity_note && <p>{card.identity_note}</p>}{card.evaluations.map((row, index) => <p key={index}>{link(row.source, data.benchmarks[row.bench].name)} · {row.date ?? '—'} · {byLabel(row.by)}</p>)}</section>)}
          {Object.entries(data.sources).map(([id, source]) => <p key={id}>{link(source.terms_url, source.name)} · {link(source.license_url, source.license)}<small>{source.attribution}</small></p>)}
        </details>
      </>}
    </div>
  </dialog>;
}
