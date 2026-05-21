import React, { useState, useRef, useEffect, useCallback } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { formatTokenCount } from '../utils/format';
import { SlashPicker } from './SlashPicker';
import { ModelSelector } from './ModelSelector';
import { postMessage } from '../vscode';

interface WorkspaceFile {
  path: string;
  fileName: string;
  relativePath: string;
}

export function InputArea() {
  const { state, send, stop, dispatch } = useChatContext();
  const [text, setText] = useState('');
  const [showSlash, setShowSlash] = useState(false);
  const [slashFilter, setSlashFilter] = useState('');
  const [showFilePicker, setShowFilePicker] = useState(false);
  const [fileQuery, setFileQuery] = useState('');
  const [workspaceFiles, setWorkspaceFiles] = useState<WorkspaceFile[]>([]);
  const inputBoxRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileSearchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  useEffect(() => {
    function handleMessage(e: MessageEvent) {
      if (e.data?.type === 'focusInput') textareaRef.current?.focus();
      if (e.data?.type === 'workspaceFiles') {
        setWorkspaceFiles(e.data.files || []);
      }
    }
    window.addEventListener('message', handleMessage);
    return () => window.removeEventListener('message', handleMessage);
  }, []);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const sessionBody = container.closest<HTMLElement>('.session-body');
    if (!sessionBody) return;

    const updateInputInset = () => {
      sessionBody.style.setProperty('--input-inset', `${container.offsetHeight + 32}px`);
    };

    updateInputInset();
    const resizeObserver = new ResizeObserver(updateInputInset);
    resizeObserver.observe(container);
    window.addEventListener('resize', updateInputInset);

    return () => {
      resizeObserver.disconnect();
      window.removeEventListener('resize', updateInputInset);
      sessionBody.style.removeProperty('--input-inset');
    };
  }, []);

  // Close pickers when clicking outside their relevant areas
  // Capture phase so no child handler can stop this from firing
  useEffect(() => {
    if (!showFilePicker && !showSlash) return;
    if (showFilePicker) {
      requestAnimationFrame(() => fileSearchRef.current?.focus());
    }

    function closePickers(e: MouseEvent) {
      const target = e.target as Node;
      if (!document.body.contains(target)) return;
      // File picker: close when clicking anywhere outside the picker itself
      // (including the textarea — user is done selecting files)
      if (showFilePicker) {
        const insidePicker = (target as HTMLElement).closest?.('.file-picker');
        if (!insidePicker) {
          setShowFilePicker(false);
          setFileQuery('');
        }
      }
      // Slash picker: close when clicking outside input-box
      // (keep open when clicking textarea so user can keep typing)
      if (showSlash && inputBoxRef.current && !inputBoxRef.current.contains(target)) {
        setShowSlash(false);
      }
    }
    document.addEventListener('mousedown', closePickers, true);
    return () => document.removeEventListener('mousedown', closePickers, true);
  }, [showFilePicker, showSlash]);

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
    if (!trimmed) return;
    send(trimmed);
    setText('');
    setShowSlash(false);
  }, [text, send]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (showSlash) return;
      if (e.nativeEvent.isComposing || e.keyCode === 229) return;
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
    setShowFilePicker((fp) => {
      if (fp) { setFileQuery(''); return false; }
      return fp;
    });
    setShowSlash((open) => {
      if (open) { setText(''); return false; }
      setText('/');
      setSlashFilter('');
      return true;
    });
    textareaRef.current?.focus();
  }, []);

  // ── File picker ──────────────────────────────────────────────

  const handleAttachClick = useCallback(() => {
    setShowFilePicker((prev) => {
      const next = !prev;
      if (next) {
        setShowSlash(false);
        postMessage({ type: 'searchWorkspaceFiles', query: '' });
      } else {
        setFileQuery('');
      }
      return next;
    });
  }, []);

  const handleFileSearch = useCallback((query: string) => {
    setFileQuery(query);
    postMessage({ type: 'searchWorkspaceFiles', query });
  }, []);

  const handleFileSelect = useCallback((f: WorkspaceFile) => {
    postMessage({ type: 'attachFile', path: f.path });
    setShowFilePicker(false);
    setFileQuery('');
    textareaRef.current?.focus();
  }, []);

  const hasText = Boolean(text.trim());

  return (
    <div className="input-container" ref={containerRef}>
      <div className="input-box" ref={inputBoxRef}>
        {showSlash && (
          <SlashPicker filter={slashFilter} onSelect={handleSlashSelect} onClose={() => setShowSlash(false)} />
        )}
        {showFilePicker && (
          <div className="file-picker">
            <input
              ref={fileSearchRef}
              className="file-picker-search"
              type="text"
              placeholder="Search project files..."
              value={fileQuery}
              onChange={(e) => handleFileSearch(e.target.value)}
            />
            <div className="file-picker-list">
              {workspaceFiles.length === 0 ? (
                <div className="file-picker-empty">
                  {fileQuery ? 'No matching files' : 'Type to search workspace files'}
                </div>
              ) : (
                workspaceFiles.map((f) => (
                  <button
                    key={f.path}
                    type="button"
                    className="file-picker-item"
                    onClick={() => handleFileSelect(f)}
                  >
                    <span className="file-picker-item-icon">📄</span>
                    <span className="file-picker-item-body">
                      <span className="file-picker-item-name">{f.fileName}</span>
                      <span className="file-picker-item-path">{f.relativePath}</span>
                    </span>
                  </button>
                ))
              )}
            </div>
          </div>
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
        />
        <div className="input-footer">
          <button className="footer-slash-btn" onClick={handleSlashButton} title="Commands">
            /
          </button>
          <button className="footer-attach-btn" onClick={handleAttachClick} title="Attach file">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
            </svg>
          </button>
          <span className="footer-spacer" />
          {state.tokenCount && <span className="footer-tokens">{formatTokenCount(state.tokenCount.total)}</span>}
          <ModelSelector placement="up" onOpen={() => setShowSlash(false)} />
          {state.isGenerating ? (
            <>
              {hasText && (
                <button className="btn-send" onClick={handleSend} title="Queue message">
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
                    <line x1="12" y1="19" x2="12" y2="5" /><polyline points="5 12 12 5 19 12" />
                  </svg>
                </button>
              )}
              <button className="btn-stop" onClick={stop} title="Stop">
                <div style={{ width: 8, height: 8, background: 'currentColor', borderRadius: 1 }} />
              </button>
            </>
          ) : (
            <button className="btn-send" onClick={handleSend} disabled={!hasText} title="Send">
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
