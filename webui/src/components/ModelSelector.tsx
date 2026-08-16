import { useState, useEffect, useRef } from 'preact/hooks';
import { getModels, ModelInfo, postLiveReasoningEffort } from '../api';
import { useT } from '../settings';
import { MsgKey } from '../i18n';

// Endpoint-declared reasoning effort levels. `null` returns to the model's
// own default while retaining its configured capability.
const EFFORT_OPTIONS: { val: string | null; key: MsgKey }[] = [
  { val: null, key: 'effort.default' },
  { val: 'low', key: 'effort.low' },
  { val: 'medium', key: 'effort.medium' },
  { val: 'high', key: 'effort.high' },
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
  onChange,
  onDefaultChange,
}: {
  value: string | null;
  onChange: (p: string) => void;
  onDefaultChange?: (p: string) => void;
}) {
  const t = useT();
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [open, setOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);
  // undefined = "show the provider's persisted value"; a string/null is a
  // local override after the user picks one (config isn't re-fetched).
  const [effortOverride, setEffortOverride] = useState<string | null | undefined>(undefined);
  const ref = useRef<HTMLDivElement>(null);
  const effortRef = useRef<HTMLDivElement>(null);

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
  useEffect(() => {
    if (!open && !effortOpen) return;
    const h = (e: MouseEvent) => {
      const tgt = e.target as Node;
      if (ref.current && !ref.current.contains(tgt)) setOpen(false);
      if (effortRef.current && !effortRef.current.contains(tgt)) setEffortOpen(false);
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [open, effortOpen]);
  const current = models.find((m) => m.provider === value) ?? models.find((m) => m.is_default) ?? models[0];
  // Switching models resets the effort display back to the new model's
  // persisted value (and hides the selector entirely for non-V4 models).
  useEffect(() => { setEffortOverride(undefined); }, [current?.provider]);
  const effort = effortOverride !== undefined ? effortOverride : (current?.reasoning_effort ?? null);
  const effortLabel = (v: string | null): string => {
    const o = EFFORT_OPTIONS.find((x) => x.val === v) ?? EFFORT_OPTIONS[0];
    return t(o.key);
  };
  const selectEffort = (v: string | null) => {
    const previous = effort;
    setEffortOverride(v);
    setEffortOpen(false);
    if (current) {
      void postLiveReasoningEffort(v, current.provider).catch((error) => {
        setEffortOverride(previous);
        console.error('Failed to update reasoning effort', error);
      });
    }
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
    <div class="model-controls">
      {current?.effort_applicable && (
        <div class="model-selector effort-selector model-selector-up" ref={effortRef}>
          <button
            class="model-selector-trigger"
            onClick={() => { setEffortOpen((o) => !o); setOpen(false); }}
            type="button"
            title={t('effort.label')}
          >
            <span class="effort-prefix">{t('effort.label')}</span>
            <span class="model-selector-label">{effortLabel(effort)}</span>
            <span class="model-selector-chevron">▾</span>
          </button>
          {effortOpen && (
            <div class="model-dropdown">
              {EFFORT_OPTIONS.filter(
                (o) =>
                  o.val === null ||
                  !current?.effort_levels?.length ||
                  current.effort_levels.includes(o.val),
              ).map((o) => (
                <button
                  key={o.key}
                  class={'model-item' + (o.val === effort ? ' active' : '')}
                  type="button"
                  onClick={() => selectEffort(o.val)}
                >
                  <span class="model-item-model">{t(o.key)}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      <div class="model-selector model-selector-up" ref={ref}>
        <button class="model-selector-trigger" onClick={() => { setOpen((o) => !o); setEffortOpen(false); }} type="button">
          <span class="model-selector-label">{current ? current.model : t('model.label')}</span>
          {current && isDup(current.model) && (
            <span class="model-selector-provider">{providerLabel(current)}</span>
          )}
          <span class="model-selector-chevron">▾</span>
        </button>
        {open && (
          <div class="model-dropdown">
            {models.map((m) => (
              <button
                key={m.provider}
                class={'model-item' + (m.provider === (value ?? current?.provider) ? ' active' : '')}
                type="button"
                title={m.provider}
                onClick={() => { onChange(m.provider); setOpen(false); }}
              >
                <span class="model-item-model">{m.model}</span>
                {isDup(m.model) && <span class="model-item-provider">{providerLabel(m)}</span>}
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
