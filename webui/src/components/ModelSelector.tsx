import { useState, useEffect, useRef } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { getModels, ModelInfo, postLiveReasoningEffort, ReasoningEffortError } from '../api';
import { useT } from '../settings';
import { displayedModel, type RuntimeModelIdentity } from '../lib/modelIdentity';
import { MsgKey } from '../i18n';
import { useDropdown } from '../lib/useDropdown';

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
  liveEffort,
  busy = false,
  onError,
  onChange,
  onDefaultChange,
}: {
  value: string | null;
  runtime?: RuntimeModelIdentity | null;
  liveEffort?: string | null;
  busy?: boolean;
  onError?: (message: string) => void;
  onChange: (p: string) => void;
  onDefaultChange?: (p: string) => void;
}) {
  const t = useT();
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [open, setOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);
  const [effortSaving, setEffortSaving] = useState(false);
  const [effortError, setEffortError] = useState<string | null>(null);
  // undefined = "show the provider's persisted value"; a string/null is a
  // local override after the user picks one (config isn't re-fetched).
  const [effortOverride, setEffortOverride] = useState<string | null | undefined>(undefined);
  // The two menus are portaled to <body> and placed against the viewport, so
  // a wide list next to a narrow trigger stays on screen and sits flush with
  // its trigger (see `useDropdown`).
  const modelDropdown = useDropdown({
    open,
    onClose: () => setOpen(false),
    prefer: 'above',
    width: 260,
  });
  const effortDropdown = useDropdown({
    open: effortOpen,
    onClose: () => setEffortOpen(false),
    prefer: 'above',
    width: 200,
  });

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
  // Outside-click and placement are `useDropdown`'s job here (both menus are
  // portaled to <body>), so there is no ref/handler pair to keep in this file.
  const current = displayedModel(models, value, runtime);
  const currentProvider = useRef(current?.provider);
  currentProvider.current = current?.provider;
  useEffect(() => { if (busy) setEffortOpen(false); }, [busy]);
  // Switching models resets the effort display back to the new model's
  // persisted value; capability declarations decide whether the selector appears.
  useEffect(() => { setEffortOverride(undefined); }, [current?.provider, current?.model]);
  useEffect(() => {
    if (liveEffort !== undefined) setEffortOverride(liveEffort);
  }, [liveEffort]);
  const effort = effortOverride !== undefined ? effortOverride : (current?.reasoning_effort ?? null);
  const effortLabel = (v: string | null): string => {
    const o = EFFORT_OPTIONS.find((x) => x.val === v) ?? EFFORT_OPTIONS[0];
    return t(o.key);
  };
  const selectEffort = (v: string | null) => {
    if (busy || effortSaving || !current) return;
    const target = current.provider;
    const previous = effort;
    setEffortError(null);
    setEffortSaving(true);
    setEffortOverride(v);
    setEffortOpen(false);
    void postLiveReasoningEffort(v, target).then(() => {
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
    <div class="model-controls">
      {effortError && <span role="alert">{effortError}</span>}
      {current?.effort_applicable && (
        <div class="model-selector effort-selector model-selector-up">
          <button
            ref={effortDropdown.triggerRef}
            class="model-selector-trigger"
            onClick={() => { setEffortOpen((o) => !o); setOpen(false); }}
            type="button"
            disabled={busy || effortSaving}
            title={t(busy ? 'effort.busy' : 'effort.label')}
          >
            <span class="effort-prefix">{t('effort.label')}</span>
            <span class="model-selector-label">{effortLabel(effort)}</span>
            <span class="model-selector-chevron">▾</span>
          </button>
          {effortOpen && !busy && !effortSaving && createPortal(
            <div
              ref={effortDropdown.menuRef}
              class="model-dropdown model-dropdown-viewport"
              style={effortDropdown.style}
            >
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
            </div>,
            document.body,
          )}
        </div>
      )}
      <div class="model-selector model-selector-up">
        <button
          ref={modelDropdown.triggerRef}
          class="model-selector-trigger"
          onClick={() => { setOpen((o) => !o); setEffortOpen(false); }}
          type="button"
        >
          <span class="model-selector-label">{current ? current.model : t('model.label')}</span>
          {current && isDup(current.model) && (
            <span class="model-selector-provider">{providerLabel(current)}</span>
          )}
          <span class="model-selector-chevron">▾</span>
        </button>
        {open && createPortal(
          <div
            ref={modelDropdown.menuRef}
            class="model-dropdown model-dropdown-viewport"
            style={modelDropdown.style}
          >
            {models.map((m) => (
              <button
                key={m.provider}
                class={'model-item' + (m.provider === current?.provider && m.model === current?.model ? ' active' : '')}
                type="button"
                title={m.provider}
                onClick={() => { onChange(m.provider); setOpen(false); }}
              >
                <span class="model-item-model">{m.model}</span>
                {isDup(m.model) && <span class="model-item-provider">{providerLabel(m)}</span>}
              </button>
            ))}
          </div>,
          document.body,
        )}
      </div>
    </div>
  );
}
