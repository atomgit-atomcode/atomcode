import React, { useState, useRef, useEffect, useCallback } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { formatTokenCount } from '../utils/format';
import { SlashPicker } from './SlashPicker';
import { ModelSelector } from './ModelSelector';

export function InputArea() {
  const { state, send, stop, dispatch } = useChatContext();
  const [text, setText] = useState('');
  const [showSlash, setShowSlash] = useState(false);
  const [slashFilter, setSlashFilter] = useState('');
  const inputBoxRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  useEffect(() => {
    function handleMessage(e: MessageEvent) {
      if (e.data?.type === 'focusInput') textareaRef.current?.focus();
    }
    window.addEventListener('message', handleMessage);
    return () => window.removeEventListener('message', handleMessage);
  }, []);

  useEffect(() => {
    if (!showSlash) return undefined;

    function handlePointerDown(e: MouseEvent) {
      if (inputBoxRef.current && !inputBoxRef.current.contains(e.target as Node)) {
        setShowSlash(false);
      }
    }

    document.addEventListener('mousedown', handlePointerDown);
    return () => document.removeEventListener('mousedown', handlePointerDown);
  }, [showSlash]);

  const handleChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setText(val);
    if (/^\/\S*$/.test(val)) {
      setSlashFilter(val.slice(1).split(/\s/)[0]);
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
      if (showSlash) return;
      if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleSend(); }
    },
    [handleSend, showSlash],
  );

  const handleSlashSelect = useCallback((command: string) => {
    setText(command + ' ');
    setShowSlash(false);
    textareaRef.current?.focus();
  }, []);

  const handleSlashButton = useCallback(() => {
    setText('/');
    setSlashFilter('');
    setShowSlash((open) => !open);
    textareaRef.current?.focus();
  }, []);

  return (
    <div className="input-container">
      <div className="input-box" ref={inputBoxRef}>
        {showSlash && (
          <SlashPicker filter={slashFilter} onSelect={handleSlashSelect} onClose={() => setShowSlash(false)} />
        )}
        {state.contextFiles.length > 0 && (
          <div className="attached-files">
            {state.contextFiles.map((f) => (
              <span key={f.path} className="attached-file-pill" title={f.path}>
                {f.type === 'selection' ? '📋' : '📄'} {f.fileName}
                <button className="pill-close" onClick={() => dispatch({ type: 'REMOVE_CONTEXT_FILE', path: f.path })}>×</button>
              </span>
            ))}
          </div>
        )}
        <textarea
          ref={textareaRef}
          className="message-input"
          value={text}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
          placeholder="Type a message..."
          rows={1}
          disabled={state.isGenerating}
        />
        <div className="input-footer">
          <button className="footer-slash-btn" onClick={handleSlashButton} title="Commands">
            /
          </button>
          <span className="footer-spacer" />
          {state.tokenCount && <span className="footer-tokens">{formatTokenCount(state.tokenCount.total)}</span>}
          <ModelSelector placement="up" onOpen={() => setShowSlash(false)} />
          {state.isGenerating ? (
            <button className="btn-stop" onClick={stop} title="Stop">
              <div style={{ width: 8, height: 8, background: 'currentColor', borderRadius: 1 }} />
            </button>
          ) : (
            <button className="btn-send" onClick={handleSend} disabled={!text.trim()} title="Send">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
                <line x1="12" y1="19" x2="12" y2="5" /><polyline points="5 12 12 5 19 12" />
              </svg>
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
