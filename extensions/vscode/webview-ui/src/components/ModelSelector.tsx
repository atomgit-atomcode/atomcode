import React, { useState, useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';

export function ModelSelector() {
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

  function handleSelect(model: string) {
    selectModel(model);
    setOpen(false);
  }

  const currentLabel =
    state.models.find((m) => m.model === state.currentModel)?.model ?? state.currentModel;

  return (
    <div className="model-selector" ref={containerRef}>
      <button
        className="model-selector-trigger"
        onClick={() => setOpen(!open)}
        title="Select model"
      >
        <span className="model-selector-label">{currentLabel}</span>
        <span className="model-selector-chevron">{open ? '▴' : '▾'}</span>
      </button>
      {open && (
        <div className="model-dropdown">
          {state.models.length === 0 && (
            <div className="model-item model-item-empty">No models available</div>
          )}
          {state.models.map((m) => (
            <button
              key={m.model}
              className={`model-item${m.model === state.currentModel ? ' active' : ''}`}
              onClick={() => handleSelect(m.model)}
            >
              <span>{m.model}</span>
              {m.is_default && <span className="model-default-badge">default</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
