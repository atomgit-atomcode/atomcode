import { ModelCardDialog } from './ModelCardDialog';
import { useState, useEffect, useLayoutEffect, useRef } from 'preact/hooks';
import { getModels, ModelInfo, postLiveReasoningEffort, ReasoningEffortError } from '../api';
import { useT } from '../settings';
import { displayedModel, type RuntimeModelIdentity } from '../lib/modelIdentity';
import { MsgKey } from '../i18n';

// Endpoint-declared reasoning effort levels. `null` returns to the model's
// own default while retaining its configured capability.
const EFFORT_OPTIONS: { val: string | null; key: MsgKey }[] = [
  { val: null, key: 'effort.default' },
  { val: 'low', key: 'effort.low' },
  { val: 'medium', key: 'effort.medium' },
  { val: 'high', key: 'effort.high' },
  { val: 'xhigh', key: 'effort.xhigh' },
  { val: 'max', key: 'effort.max' },
];

function areModelsEqual(a: ModelInfo[], b: ModelInfo[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const ma = a[i];
    const mb = b[i];
    if (
      ma.provider !== mb.provider ||
      ma.model !== mb.model ||
      ma.is_default !== mb.is_default ||
      ma.model_card_id !== mb.model_card_id ||
      ma.model_card_status !== mb.model_card_status ||
      ma.context_window !== mb.context_window ||
      ma.supports_vision !== mb.supports_vision ||
      ma.effort_applicable !== mb.effort_applicable ||
      ma.reasoning_effort !== mb.reasoning_effort ||
      (ma.effort_levels ?? []).join(',') !== (mb.effort_levels ?? []).join(',')
    ) {
      return false;
    }
  }
  return true;
}

export function ModelSelector({
  value,
  runtime,
  currentTurn,
  liveEffort,
  busy = false,
  onError,
  onChange,
  onDefaultChange,
  onEffortChange,
}: {
  value: string | null;
  runtime?: RuntimeModelIdentity | null;
  currentTurn?: RuntimeModelIdentity | null;
  liveEffort?: string | null;
  busy?: boolean;
  onError?: (message: string) => void;
  onChange: (p: string) => void;
  onDefaultChange?: (p: string) => void;
  onEffortChange?: (provider: string, effort: string | null) => void;
}) {
  const t = useT();
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [open, setOpen] = useState(false);
  const [cardModel, setCardModel] = useState<ModelInfo | null>(null);
  const [effortOpen, setEffortOpen] = useState(false);
  const [effortSaving, setEffortSaving] = useState(false);
  const [effortError, setEffortError] = useState<string | null>(null);
  // undefined = "show the provider's persisted value"; a string/null is a
  // local override after the user picks one (config isn't re-fetched).
  const [effortOverride, setEffortOverride] = useState<string | null | undefined>(undefined);
  const ref = useRef<HTMLDivElement>(null);
  const [effortDraft, setEffortDraft] = useState<string | null | undefined>(undefined);

  const [popupStyle, setPopupStyle] = useState<{position: 'fixed'; left: string; right: string; width: string; top: string; bottom: string; maxHeight: string} | undefined>();
  useLayoutEffect(() => {
    if (!open && !effortOpen) return;
    const position = () => {
      if (!ref.current) return;
      const rect = (ref.current.querySelector('.model-selector-trigger') ?? ref.current).getBoundingClientRect();
      const viewport = window.visualViewport;
      const visibleTop = viewport?.offsetTop ?? 0;
      const visibleBottom = visibleTop + (viewport?.height ?? window.innerHeight);
      const visibleLeft = viewport?.offsetLeft ?? 0;
      const visibleWidth = viewport?.width ?? window.innerWidth;
      const width = Math.max(0, Math.min(effortOpen ? 280 : 350, visibleWidth - 24));
      const left = Math.max(visibleLeft + 12, Math.min(rect.left + rect.width / 2 - width / 2, visibleLeft + visibleWidth - width - 12));
      const above = Math.max(0, rect.top - visibleTop - 20);
      const below = Math.max(0, visibleBottom - rect.bottom - 20);
      const base = {position: 'fixed' as const, left: `${left}px`, right: 'auto', width: `${width}px`};
      setPopupStyle(above >= (effortOpen ? 170 : 280) || above >= below
        ? {...base, top: 'auto', bottom: `${window.innerHeight - rect.top + 8}px`, maxHeight: `${Math.min(280, above)}px`}
        : {...base, top: `${rect.bottom + 8}px`, bottom: 'auto', maxHeight: `${Math.min(280, below)}px`});
    };
    position();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(position);
    if (ref.current) observer?.observe(ref.current.querySelector('.model-selector-trigger') ?? ref.current);
    window.addEventListener('resize', position);
    window.addEventListener('scroll', position, true);
    window.visualViewport?.addEventListener('resize', position);
    window.visualViewport?.addEventListener('scroll', position);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', position);
      window.removeEventListener('scroll', position, true);
      window.visualViewport?.removeEventListener('resize', position);
      window.visualViewport?.removeEventListener('scroll', position);
    };
  }, [open, effortOpen]);

  const modelsRef = useRef(models);
  modelsRef.current = models;
  const openRef = useRef(open);
  openRef.current = open;
  const effortOpenRef = useRef(effortOpen);
  effortOpenRef.current = effortOpen;

  useEffect(() => {
    let active = true;
    const refresh = () => {
      if (openRef.current || effortOpenRef.current) return;
      getModels().then((next) => {
        if (!active) return;
        if (openRef.current || effortOpenRef.current) return;
        if (!areModelsEqual(modelsRef.current, next)) {
          setModels(next);
          const defaultModel = next.find((model) => model.is_default) ?? next[0];
          if (defaultModel) onDefaultChange?.(defaultModel.provider);
        }
      }).catch(() => {});
    };
    refresh();
    const timer = window.setInterval(refresh, 2_000);
    const onVisibility = () => {
      if (document.visibilityState === 'visible') refresh();
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      active = false;
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [onDefaultChange]);
  useLayoutEffect(() => {
    if (!open && !effortOpen) return;
    const h = (e: MouseEvent) => {
      const tgt = e.target as Node;
      if (ref.current && !ref.current.contains(tgt)) { setOpen(false); setEffortOpen(false); }
    };
    const escape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') { setOpen(false); setEffortOpen(false); ref.current?.querySelector<HTMLButtonElement>('.model-selector-trigger')?.focus(); }
    };
    document.addEventListener('mousedown', h);
    document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('mousedown', h); document.removeEventListener('keydown', escape); };
  }, [open, effortOpen]);
  const current = displayedModel(models, value, runtime);
  const currentProvider = useRef(current?.provider);
  currentProvider.current = current?.provider;
  useEffect(() => { setEffortDraft(undefined); }, [effortOpen, current?.provider, liveEffort]);
  useLayoutEffect(() => {
    if (effortOpen) ref.current?.querySelector<HTMLButtonElement>('.model-effort-name')?.focus();
  }, [effortOpen]);
  // Switching models resets the effort display back to the new model's
  // persisted value; capability declarations decide whether the selector appears.
  useEffect(() => { setEffortOverride(undefined); }, [current?.provider, current?.model]);
  useEffect(() => {
    if (liveEffort !== undefined) setEffortOverride(liveEffort);
  }, [liveEffort]);
  const effort = effortOverride !== undefined ? effortOverride : (current?.reasoning_effort ?? null);
  const effortLabel = (v: string | null): string => {
    const o = EFFORT_OPTIONS.find((x) => x.val === v) ?? EFFORT_OPTIONS[0];
    return o.val === v ? t(o.key) : (v ?? t('effort.default'));
  };
  const selectEffort = (v: string | null) => {
    if (busy || effortSaving || !current) return;
    const target = current.provider;
    const previous = effort;
    setEffortError(null);
    setEffortSaving(true);
    setEffortOverride(v);
    setEffortDraft(undefined);
    void postLiveReasoningEffort(v, target).then(() => {
      if (currentProvider.current === target) onEffortChange?.(target, v);
      // The write succeeded. A later list refresh failure must not undo that fact.
      void getModels().then(setModels).catch(() => {});
    }).catch((error) => {
      if (currentProvider.current === target) setEffortOverride(previous);
      const message = t(error instanceof ReasoningEffortError && error.activeTurn ? 'effort.busy' : 'effort.failed');
      if (onError) onError(message);
      else setEffortError(message);
    }).finally(() => setEffortSaving(false));
  };
  // 同名模型可能来自多个 Provider（如两个 deepseek-v4-flash）。仅在模型名重复时
  // 附上 Provider 标识以区分，唯一的模型名保持简洁。
  const modelCounts = new Map<string, number>();
  for (const m of models) modelCounts.set(m.model, (modelCounts.get(m.model) ?? 0) + 1);
  const isDup = (name: string) => (modelCounts.get(name) ?? 0) > 1;
  // Provider 名常形如 "AtomGit-deepseek-v4-flash"（厂商前缀 + 模型名）。去掉其中
  // 重复的模型名片段，得到简短厂商标识（→ "AtomGit"）；不含模型名的原样返回（→ "DeepSeek"）。
  const providerLabel = (m: ModelInfo): string => {
    const i = m.provider.indexOf(m.model);
    if (i < 0) return m.provider;
    const stripped = (m.provider.slice(0, i) + m.provider.slice(i + m.model.length))
      .replace(/^[-_/\s]+|[-_/\s]+$/g, '');
    return stripped || m.provider;
  };
  return (
    <div class={'model-controls' + (currentTurn ? ' has-pending-model' : '')}>
      {cardModel && <ModelCardDialog model={cardModel} onClose={() => setCardModel(null)} />}
      {effortError && <span role="alert">{effortError}</span>}
      {currentTurn && current && <span class="model-next-turn" role="status">{t('model.nextTurnStatus', {current:currentTurn.model??currentTurn.provider, next:current.model})}</span>}
      <div class="model-selector unified-model-selector model-selector-up" ref={ref}>
        <button class="model-selector-trigger" aria-expanded={open || effortOpen}
          onClick={() => {
            if (open || effortOpen) { setOpen(false); setEffortOpen(false); }
            else if (current?.effort_applicable) setEffortOpen(true);
            else setOpen(true);
          }} type="button">
          <span class="model-selector-label">{current ? current.model : t('model.label')}{currentTurn && <span class="model-next-turn-badge"> · {t('model.nextTurn')}</span>}</span>
          {current && isDup(current.model) && <span class="model-selector-provider">{providerLabel(current)}</span>}
          {current?.effort_applicable && effort !== null && <span class="model-effort-summary">{effortLabel(effort)}</span>}
          <span class="model-selector-chevron" aria-hidden="true">▾</span>
        </button>
        {effortOpen && current && (
          <div class="model-dropdown model-effort-panel" style={popupStyle}>
            <div class="model-effort-heading">
              <span>{t(current.effort_applicable ? 'effort.label' : 'model.label')}</span>
              {current.effort_applicable && <strong aria-live="polite">{effortLabel(effortDraft !== undefined ? effortDraft : effort)}</strong>}
            </div>
            <button class="model-effort-name" type="button" onClick={() => { ref.current?.querySelector<HTMLButtonElement>('.model-selector-trigger')?.focus(); setEffortOpen(false); setOpen(true); }}>
              <span>{current.model}{isDup(current.model) ? ` (${providerLabel(current)})` : ''}</span>
              <span aria-hidden="true">›</span>
            </button>
            {current.effort_applicable && (() => {
              // The server owns the order, including levels newer than this UI.
              const options = current.effort_levels?.length
                ? [{val: null}, ...Array.from(new Set(current.effort_levels)).map(val => ({val}))]
                : EFFORT_OPTIONS;
              const preview = effortDraft !== undefined ? effortDraft : effort;
              const index = Math.max(0, options.findIndex(o => o.val === preview));
              return <div class="model-effort-slider" style={{'--effort-count': options.length}}>
                <input type="range" min="0" max={options.length - 1} step="1" value={index}
                  aria-label={t('effort.label')} aria-valuetext={effortLabel(preview)}
                  disabled={busy || options.length < 2} aria-disabled={busy || effortSaving} aria-busy={effortSaving}
                  onInput={e => {
                    if (effortSaving) { e.currentTarget.value = String(index); return; }
                    setEffortDraft(options[Number(e.currentTarget.value)].val);
                  }}
                  onChange={e => {
                    if (effortSaving) { e.currentTarget.value = String(index); return; }
                    selectEffort(options[Number(e.currentTarget.value)].val);
                  }} />
                <div class="model-effort-stops" role="group" aria-label={t('effort.label')}>
                  {options.map((o, i) => <button key={o.val ?? 'default'} type="button"
                    class={i === index ? 'active' : ''} aria-pressed={i === index}
                    disabled={busy || effortSaving} onClick={() => selectEffort(o.val)}>{effortLabel(o.val)}</button>)}
                </div>
                {busy && <p class="model-effort-hint">{t('effort.busy')}</p>}
              </div>;
            })()}
            {!current.effort_applicable && <p class="model-effort-hint">{t('effort.unavailable')}</p>}
          </div>
        )}
        {open && (
          <div class="model-dropdown model-choice-list" style={popupStyle}>
            {models.map((m) => (
              <div class="model-card-row" key={m.provider}><button
                class={'model-item' + (m.provider === current?.provider && m.model === current?.model ? ' active' : '')}
                type="button"
                title={m.provider}
                onClick={() => { onChange(m.provider); setOpen(false); setEffortOpen(true); }}
              >
                <span class="model-item-model">{m.model}</span>
                {m.provider === current?.provider && m.model === current?.model && <span class="model-choice-check" aria-hidden="true">✓</span>}
                {isDup(m.model) && <span class="model-item-provider">{providerLabel(m)}</span>}
              </button><button type="button" class="model-card-info" aria-label={`${t('cards.open')}: ${m.model}`} title={t('cards.open')} onClick={() => { setCardModel(m); setOpen(false); }}>{m.model_card_status === 'no_public_data' ? t('cards.noPublicBadge') : 'ⓘ'}</button></div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
