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

  const handleChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setText(val);
    if (val.startsWith('/')) {
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

  return (
    <div className="input-container">
      <div className="input-box">
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
          <button className="footer-attach-btn" onClick={() => postMessage({ type: 'attachFile' })} title="Attach file">
            <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
              <path d="M11.5 1A3.5 3.5 0 0115 4.5v6a4.5 4.5 0 01-9 0V4a2 2 0 114 0v6.5a.5.5 0 01-1 0V4H7.5v6.5a2 2 0 004 0V4.5a2 2 0 00-4 0v6a3 3 0 006 0v-6A3.5 3.5 0 0011.5 1z" />
            </svg>
            Attach
          </button>
          <span className="footer-spacer" />
          {state.tokenCount && <span className="footer-tokens">{formatTokenCount(state.tokenCount.total)}</span>}
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
