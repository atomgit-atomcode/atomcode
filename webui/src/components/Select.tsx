import { ComponentChildren } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';

export interface SelectOption<T extends string> {
  value: T;
  label: ComponentChildren;
}

export function Select<T extends string>({
  value,
  options,
  onChange,
  class: className,
  disabled,
}: {
  value: T;
  options: SelectOption<T>[];
  onChange: (v: T) => void;
  class?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const selected = options.find((o) => o.value === value);

  // Click outside to close
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [open]);

  return (
    <div ref={ref} class={'custom-select' + (className ? ' ' + className : '')}>
      <button
        type="button"
        class={'custom-select-trigger' + (open ? ' open' : '')}
        disabled={disabled}
        onClick={() => !disabled && setOpen(!open)}
      >
        <span class="custom-select-value">{selected?.label ?? value}</span>
        <svg class="custom-select-arrow" width="12" height="12" viewBox="0 0 24 24" fill="none"
          stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="6 9 12 15 18 9" />
        </svg>
      </button>
      {open && (
        <div class="custom-select-dropdown">
          {options.map((o) => (
            <div
              key={o.value}
              class={'custom-select-option' + (o.value === value ? ' selected' : '')}
              onClick={() => {
                onChange(o.value);
                setOpen(false);
              }}
            >
              {o.label}
              {o.value === value && (
                <svg class="custom-select-check" width="12" height="12" viewBox="0 0 24 24" fill="none"
                  stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                  <polyline points="20 6 9 17 4 12" />
                </svg>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
