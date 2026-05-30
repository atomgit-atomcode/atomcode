import { useState, useEffect, useRef } from 'preact/hooks';
import { getModels, ModelInfo } from '../api';

export function ModelSelector({ value, onChange }: { value: string | null; onChange: (p: string) => void }) {
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
  return (
    <div class="model-selector model-selector-up" ref={ref}>
      <button class="model-selector-trigger" onClick={() => setOpen((o) => !o)} type="button">
        <span class="model-selector-label">{current ? current.model : '模型'}</span>
        <span class="model-selector-chevron">▾</span>
      </button>
      {open && (
        <div class="model-dropdown">
          {models.map((m) => (
            <button
              key={m.provider}
              class={'model-item' + (m.provider === (value ?? current?.provider) ? ' active' : '')}
              type="button"
              onClick={() => { onChange(m.provider); setOpen(false); }}
            >
              <span class="model-item-model">{m.model}</span>
              <span class="model-item-provider">{m.provider}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
