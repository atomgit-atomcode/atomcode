import { useState, useEffect, useRef } from 'preact/hooks';
import { ApprovalMode } from '../api';
import { useT } from '../settings';
import { MsgKey } from '../i18n';

// Approval modes shown in the pill dropdown. Order = display order.
// The Auto mode keeps the `bypass` wire value for protocol compatibility and
// remains safety-consequential, so it is styled with a warning accent.
const MODE_OPTIONS: { val: ApprovalMode; label: MsgKey; desc: MsgKey }[] = [
  { val: 'build', label: 'mode.build', desc: 'mode.build.desc' },
  { val: 'accept_edits', label: 'mode.accept_edits', desc: 'mode.accept_edits.desc' },
  { val: 'bypass', label: 'mode.auto', desc: 'mode.auto.desc' },
  { val: 'plan', label: 'mode.plan', desc: 'mode.plan.desc' },
];

export function ModeSelector({
  value,
  disabled = false,
  onChange,
}: {
  value: ApprovalMode;
  disabled?: boolean;
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
  const isAuto = value === 'bypass';
  return (
    <div class="model-selector model-selector-up mode-selector" ref={ref}>
      <button
        class={'model-selector-trigger mode-selector-trigger' + (isAuto ? ' mode-bypass' : '')}
        onClick={() => {
          if (!disabled) setOpen((o) => !o);
        }}
        disabled={disabled}
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
              disabled={disabled}
              onClick={() => {
                if (disabled) return;
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
