import React, { useState, useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { useT } from '../i18n';

type ModelSelectorProps = {
  placement?: 'up' | 'down';
  onOpen?: () => void;
};

export function ModelSelector({ placement = 'down', onOpen }: ModelSelectorProps) {
  const { state, selectModel, selectReasoningEffort } = useChatContext();
  const t = useT();
  const [open, setOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const effortRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
      if (effortRef.current && !effortRef.current.contains(e.target as Node)) {
        setEffortOpen(false);
      }
    }
    if (open || effortOpen) {
      document.addEventListener('mousedown', handleClickOutside);
    }
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [open, effortOpen]);

  function handleSelect(provider: string, model: string) {
    selectModel(provider, model);
    setOpen(false);
    setEffortOpen(false);
  }

  function handleTriggerClick() {
    if (!open) onOpen?.();
    setOpen(!open);
    setEffortOpen(false);
  }

  function handleEffortSelect(effort: string | null) {
    if (!currentEffortModel) return;
    selectReasoningEffort(currentEffortModel.provider, effort);
    setEffortOpen(false);
  }

  const currentProvider = state.providers.find((p) => p.name === state.currentProvider);
  const currentLabel =
    currentProvider?.model
    ?? state.models.find((m) => m.provider === state.currentProvider)?.model
    ?? state.currentModel;
  const loginRequired = currentProvider?.requires_login === true
    && (!state.auth?.logged_in || state.auth.expired === true);
  const currentDisplayLabel = loginRequired
    ? `${currentLabel} · ${t('model.loginRequired')}`
    : currentLabel;

  const currentEffortModel = state.models.find((m) => m.provider === state.currentProvider);
  const showEffort = !!currentEffortModel?.effort_applicable;
  const currentEffort = currentEffortModel?.reasoning_effort ?? null;
  const effortOptions: Array<{ value: string | null; label: string }> = [
    { value: null, label: t('model.effortDefault') },
    { value: 'high', label: 'High' },
    { value: 'max', label: 'Max' },
  ];
  const effortLabel = effortOptions.find((option) => option.value === currentEffort)?.label ?? t('model.effortDefault');

  return (
    <>
      {showEffort && (
        <div className={`model-selector effort-selector model-selector-${placement}`} ref={effortRef}>
          <button
            className="model-selector-trigger"
            onClick={() => {
              if (!effortOpen) onOpen?.();
              setEffortOpen(!effortOpen);
              setOpen(false);
            }}
            title={t('model.effortTitle')}
          >
            <span className="effort-prefix">{t('model.effortLabel')}</span>
            <span className="model-selector-label">{effortLabel}</span>
            <span className="model-selector-chevron">{effortOpen ? '▴' : '▾'}</span>
          </button>
          {effortOpen && (
            <div className="model-dropdown effort-dropdown">
              {effortOptions.map((option) => (
                <button
                  key={option.value ?? 'default'}
                  className={`model-item${option.value === currentEffort ? ' active' : ''}`}
                  onClick={() => handleEffortSelect(option.value)}
                >
                  <span className="model-item-main">
                    <span>{option.label}</span>
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      <div className={`model-selector model-selector-${placement}`} ref={containerRef}>
        <button
          className="model-selector-trigger"
          onClick={handleTriggerClick}
          title={t('model.selectModel')}
        >
          <span className="model-selector-label">{currentDisplayLabel}</span>
          <span className="model-selector-chevron">{open ? '▴' : '▾'}</span>
        </button>
        {open && (
          <div className="model-dropdown">
            {state.providers.length === 0 && state.models.length === 0 && (
              <div className="model-item model-item-empty">{t('model.noModels')}</div>
            )}
            {(state.providers.length > 0
              ? state.providers.map((p) => ({
                provider: p.name,
                model: p.model,
                provider_type: p.type,
                is_default: p.is_default,
              }))
              : state.models
            ).map((m) => (
              <button
                key={`${m.provider}:${m.model}`}
                className={`model-item${m.provider === state.currentProvider ? ' active' : ''}`}
                onClick={() => handleSelect(m.provider, m.model)}
              >
                <span className="model-item-main">
                  <span>{m.model}</span>
                  <span className="model-item-provider">{m.provider}</span>
                </span>
                {m.is_default && <span className="model-default-badge">{t('model.defaultBadge')}</span>}
              </button>
            ))}
          </div>
        )}
      </div>
    </>
  );
}
