import React, { useEffect, useRef, useState } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { ApprovalMode } from '../state/types';
import { MsgKey, useT } from '../i18n';

const OPTIONS: Array<{ value: ApprovalMode; labelKey: MsgKey; descKey: MsgKey }> = [
  { value: 'build', labelKey: 'mode.build', descKey: 'mode.buildDesc' },
  { value: 'accept_edits', labelKey: 'mode.acceptEdits', descKey: 'mode.acceptEditsDesc' },
  { value: 'bypass', labelKey: 'mode.auto', descKey: 'mode.autoDesc' },
  { value: 'plan', labelKey: 'mode.plan', descKey: 'mode.planDesc' },
];

export function ModeSelector({ placement = 'up', onOpen }: { placement?: 'up' | 'down'; onOpen?: () => void }) {
  const { state, selectApprovalMode } = useChatContext();
  const t = useT();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const disabled = state.approvalModePending;

  useEffect(() => {
    if (!open) return;
    function handleClickOutside(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [open]);

  const current = OPTIONS.find((option) => option.value === state.approvalMode) ?? OPTIONS[0];

  return (
    <div className={`model-selector mode-selector model-selector-${placement}`} ref={ref}>
      <button
        className="model-selector-trigger"
        onClick={() => {
          if (disabled) return;
          if (!open) onOpen?.();
          setOpen(!open);
        }}
        disabled={disabled}
        title={t('mode.title')}
      >
        <span className="model-selector-label">{t(current.labelKey)}</span>
        <span className="model-selector-chevron">{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className="model-dropdown mode-dropdown">
          {OPTIONS.map((option) => (
            <button
              key={option.value}
              className={`model-item${option.value === state.approvalMode ? ' active' : ''}${option.value === 'bypass' ? ' danger' : ''}`}
              disabled={disabled}
              onClick={() => {
                if (disabled) return;
                selectApprovalMode(option.value);
                setOpen(false);
              }}
            >
              <span className="model-item-main">
                <span>{t(option.labelKey)}</span>
                <span className="model-item-provider">{t(option.descKey)}</span>
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
