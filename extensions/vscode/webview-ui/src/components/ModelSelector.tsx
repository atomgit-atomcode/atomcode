import React, { useState, useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';

type ModelSelectorProps = {
  placement?: 'up' | 'down';
  onOpen?: () => void;
};

export function ModelSelector({ placement = 'down', onOpen }: ModelSelectorProps) {
  const { state, selectModel } = useChatContext();
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    if (open) {
      document.addEventListener('mousedown', handleClickOutside);
    }
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [open]);

  function handleSelect(provider: string, model: string) {
    selectModel(provider, model);
    setOpen(false);
  }

  function handleTriggerClick() {
    if (!open) onOpen?.();
    setOpen(!open);
  }

  const currentLabel =
    state.providers.find((p) => p.name === state.currentProvider)?.model
    ?? state.models.find((m) => m.provider === state.currentProvider)?.model
    ?? state.currentModel;

  return (
    <div className={`model-selector model-selector-${placement}`} ref={containerRef}>
      <button
        className="model-selector-trigger"
        onClick={handleTriggerClick}
        title="Select model"
      >
        <span className="model-selector-label">{currentLabel}</span>
        <span className="model-selector-chevron">{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className="model-dropdown">
          {state.providers.length === 0 && state.models.length === 0 && (
            <div className="model-item model-item-empty">No models available</div>
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
              {m.is_default && <span className="model-default-badge">default</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
