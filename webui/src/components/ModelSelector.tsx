import { useState, useEffect, useRef } from 'preact/hooks';
import { getModels, ModelInfo } from '../api';
import { useT } from '../settings';

export function ModelSelector({ value, onChange }: { value: string | null; onChange: (p: string) => void }) {
  const t = useT();
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => { getModels().then(setModels).catch(() => {}); }, []);
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false); };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [open]);
  const current = models.find((m) => m.provider === value) ?? models.find((m) => m.is_default) ?? models[0];
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
    <div class="model-selector model-selector-up" ref={ref}>
      <button class="model-selector-trigger" onClick={() => setOpen((o) => !o)} type="button">
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
  );
}
