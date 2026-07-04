import { useState, useEffect, useRef } from 'preact/hooks';
import { ApprovalMode } from '../api';
import { useT } from '../settings';
import { MsgKey } from '../i18n';

// Approval modes shown in the pill dropdown. Order = display order.
// `bypass` (免审批) is a safety-consequential mode → styled with a warning accent.
const MODE_OPTIONS: { val: ApprovalMode; label: MsgKey; desc: MsgKey }[] = [
  { val: 'build', label: 'mode.build', desc: 'mode.build.desc' },
  { val: 'plan', label: 'mode.plan', desc: 'mode.plan.desc' },
  { val: 'bypass', label: 'mode.bypass', desc: 'mode.bypass.desc' },
];

export function ModeSelector({
  value,
  onChange,
}: {
  value: ApprovalMode;
  onChange: (m: ApprovalMode) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [open]);
  const current = MODE_OPTIONS.find((o) => o.val === value) ?? MODE_OPTIONS[0];
  const isBypass = value === 'bypass';
  return (
    <div class="model-selector model-selector-up mode-selector" ref={ref}>
      <button
        class={'model-selector-trigger mode-selector-trigger' + (isBypass ? ' mode-bypass' : '')}
        onClick={() => setOpen((o) => !o)}
        type="button"
        title={t('mode.label')}
      >
        <span class="effort-prefix">{t('mode.label')}</span>
        <span class="model-selector-label">{t(current.label)}</span>
        <span class="model-selector-chevron">▾</span>
      </button>
      {open && (
        <div class="model-dropdown mode-dropdown">
          {MODE_OPTIONS.map((o) => (
            <button
              key={o.val}
              class={
                'model-item mode-item' +
                (o.val === value ? ' active' : '') +
                (o.val === 'bypass' ? ' mode-item-bypass' : '')
              }
              type="button"
              onClick={() => {
                onChange(o.val);
                setOpen(false);
              }}
            >
              <span class="model-item-model">{t(o.label)}</span>
              <span class="mode-item-desc">{t(o.desc)}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
