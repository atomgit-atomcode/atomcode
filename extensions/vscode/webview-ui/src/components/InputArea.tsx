import React, { useState, useRef, useEffect, useCallback } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { formatTokenCount } from '../utils/format';
import { postMessage } from '../vscode';
import { SlashPicker } from './SlashPicker';

export function InputArea() {
  const { state, send, stop, dispatch } = useChatContext();
  const [text, setText] = useState('');
  const [showSlash, setShowSlash] = useState(false);
  const [slashFilter, setSlashFilter] = useState('');
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Auto-resize textarea
  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  // Listen for focusInput from extension
  useEffect(() => {
    function handleMessage(e: MessageEvent) {
      if (e.data?.type === 'focusInput') {
        textareaRef.current?.focus();
      }
    }
    window.addEventListener('message', handleMessage);
    return () => window.removeEventListener('message', handleMessage);
  }, []);

  const handleChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setText(val);

    // Slash command detection
    if (val.startsWith('/')) {
      const afterSlash = val.slice(1).split(/\s/)[0];
      setSlashFilter(afterSlash);
      setShowSlash(true);
    } else {
      setShowSlash(false);
    }
  }, []);

  const handleSend = useCallback(() => {
    const trimmed = text.trim();
    if (!trimmed || state.isGenerating) return;
    send(trimmed);
    setText('');
    setShowSlash(false);
  }, [text, state.isGenerating, send]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (showSlash) return; // Let SlashPicker handle keys
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        handleSend();
      }
    },
    [handleSend, showSlash],
  );

  const handleSlashSelect = useCallback((command: string) => {
    setText(command + ' ');
    setShowSlash(false);
    textareaRef.current?.focus();
  }, []);

  const handleSlashClose = useCallback(() => {
    setShowSlash(false);
  }, []);

  const handleRemoveContext = useCallback(
    (path: string) => {
      dispatch({ type: 'REMOVE_CONTEXT_FILE', path });
    },
    [dispatch],
  );

  const handleAttach = useCallback(() => {
    postMessage({ type: 'attachFile' });
  }, []);

  return (
    <div className="input-area">
      {state.contextFiles.length > 0 && (
        <div className="context-tags">
          {state.contextFiles.map((f) => (
            <span key={f.path} className="context-tag" title={f.path}>
              <span className="context-tag-icon">
                {f.type === 'selection' ? '📋' : '📄'}
              </span>
              <span className="context-tag-name">{f.fileName}</span>
              <button
                className="context-tag-close"
                onClick={() => handleRemoveContext(f.path)}
                title="Remove"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}

      <div className="input-wrapper">
        {showSlash && (
          <SlashPicker
            filter={slashFilter}
            onSelect={handleSlashSelect}
            onClose={handleSlashClose}
          />
        )}
        <textarea
          ref={textareaRef}
          className="input-textarea"
          value={text}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
          placeholder="Ask AtomCode..."
          rows={1}
          disabled={state.isGenerating}
        />
        <div className="input-buttons">
          {state.isGenerating ? (
            <button className="btn-stop" onClick={stop} title="Stop generation">
              <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor">
                <rect x="2" y="2" width="10" height="10" rx="1.5" />
              </svg>
            </button>
          ) : (
            <button
              className="btn-send"
              onClick={handleSend}
              disabled={!text.trim()}
              title="Send message"
            >
              <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor">
                <path d="M1.5 1.2L13 7 1.5 12.8V8.3L9 7 1.5 5.7V1.2z" />
              </svg>
            </button>
          )}
        </div>
      </div>

      <div className="input-footer">
        <button className="footer-attach-btn" onClick={handleAttach} title="Attach file">
          <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
            <path d="M11.5 1A3.5 3.5 0 0115 4.5v6a4.5 4.5 0 01-9 0V4a2 2 0 114 0v6.5a.5.5 0 01-1 0V4H7.5v6.5a2 2 0 004 0V4.5a2 2 0 00-4 0v6a3 3 0 006 0v-6A3.5 3.5 0 0011.5 1z" />
          </svg>
          <span>Attach</span>
        </button>
        <span className="footer-hint">Enter ↵ Send</span>
        {state.tokenCount && (
          <span className="footer-tokens">{formatTokenCount(state.tokenCount.total)}</span>
        )}
      </div>
    </div>
  );
}
