import React, { useState, useRef, useEffect, useCallback } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { formatTokenCount } from '../utils/format';
import { imageDataUrl } from '../utils/format';
import { SlashPicker } from './SlashPicker';
import { ModelSelector } from './ModelSelector';
import { ModeSelector } from './ModeSelector';
import { TodoPanel } from './TodoPanel';
import { postMessage } from '../vscode';
import { ImageData, SkillInfo } from '../state/types';
import { useT } from '../i18n';
import {
  applyAtMentionSelection,
  detectAtMentionRange,
  ensureActiveDescendantVisible,
} from '../utils/atMention';

interface WorkspaceFile {
  path: string;
  fileName: string;
  relativePath: string;
}

interface WorkspacePath {
  path: string;
  fileName: string;
  relativePath: string;
  isDir: boolean;
}

const MAX_IMAGES = 6;
const MAX_IMAGE_MB = 2;
const MAX_IMAGE_BYTES = MAX_IMAGE_MB * 1024 * 1024;
const MAX_INPUT_HISTORY = 100;

function fileToImageData(file: File): Promise<ImageData | null> {
  return new Promise((resolve) => {
    const reader = new FileReader();
    reader.onerror = () => resolve(null);
    reader.onload = () => {
      const value = typeof reader.result === 'string' ? reader.result : '';
      const comma = value.indexOf(',');
      if (comma < 0) {
        resolve(null);
        return;
      }
      resolve({
        media_type: file.type || 'image/png',
        data: value.slice(comma + 1),
      });
    };
    reader.readAsDataURL(file);
  });
}

export function InputArea() {
  const { state, send, stop, dispatch } = useChatContext();
  const t = useT();
  const [text, setText] = useState('');
  const [showSlash, setShowSlash] = useState(false);
  const [slashFilter, setSlashFilter] = useState('');
  const [slashSkills, setSlashSkills] = useState<SkillInfo[] | null>(null);
  const [slashLoading, setSlashLoading] = useState(false);
  const [showAttachMenu, setShowAttachMenu] = useState(false);
  const [showFilePicker, setShowFilePicker] = useState(false);
  const [showAtPicker, setShowAtPicker] = useState(false);
  const [fileQuery, setFileQuery] = useState('');
  const [workspaceFiles, setWorkspaceFiles] = useState<WorkspaceFile[]>([]);
  const [atQuery, setAtQuery] = useState('');
  const [atItems, setAtItems] = useState<WorkspacePath[]>([]);
  const [atIndex, setAtIndex] = useState(0);
  const [pendingImages, setPendingImages] = useState<ImageData[]>([]);
  const [attachError, setAttachError] = useState<string | null>(null);
  const inputBoxRef = useRef<HTMLDivElement>(null);
  const attachMenuRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileSearchRef = useRef<HTMLInputElement>(null);
  const atListRef = useRef<HTMLDivElement>(null);
  const imageInputRef = useRef<HTMLInputElement>(null);
  const inputHistoryRef = useRef<string[]>([]);
  const historyIndexRef = useRef(-1); // -1 = not navigating history; 0 = newest entry; increases going back
  const originalTextRef = useRef('');
  const textRef = useRef(text);
  const updateText = useCallback((next: string) => {
    textRef.current = next;
    setText(next);
  }, []);

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
      if (e.data?.type === 'workspacePaths') {
        setAtItems(e.data.paths || []);
        setAtIndex(0);
      }
      if (e.data?.type === 'skills') {
        setSlashSkills(e.data.skills || []);
        setSlashLoading(false);
      }
      if (e.data?.type === 'setDraft') {
        historyIndexRef.current = -1;
        updateText(e.data.text);
      }
      if (e.data?.type === 'insertText') {
        historyIndexRef.current = -1;
        insertAtCursor(e.data.text);
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
    if (!showFilePicker && !showSlash && !showAttachMenu && !showAtPicker) return;
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
      if (showAtPicker && inputBoxRef.current && !inputBoxRef.current.contains(target)) {
        setShowAtPicker(false);
      }
      if (showAttachMenu && attachMenuRef.current && !attachMenuRef.current.contains(target)) {
        setShowAttachMenu(false);
      }
    }
    document.addEventListener('mousedown', closePickers, true);
    return () => document.removeEventListener('mousedown', closePickers, true);
  }, [showAtPicker, showAttachMenu, showFilePicker, showSlash]);

  useEffect(() => {
    if (!showAtPicker) return;
    requestAnimationFrame(() => {
      const container = atListRef.current;
      const active = container?.querySelector<HTMLButtonElement>('.file-picker-item.active');
      if (container && active) ensureActiveDescendantVisible(container, active);
    });
  }, [atIndex, atItems.length, showAtPicker]);

  const ensureSlashSkills = useCallback(() => {
    if (slashSkills !== null || slashLoading) return;
    setSlashLoading(true);
    postMessage({ type: 'getSkills' });
  }, [slashLoading, slashSkills]);

  const handleChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    updateText(val);
    const cursor = e.target.selectionStart ?? val.length;
    // User manually edited — exit history navigation mode
    if (historyIndexRef.current >= 0) {
      historyIndexRef.current = -1;
    }
    if (/^\/\S*$/.test(val)) {
      setSlashFilter(val.slice(1).split(/\s/)[0]);
      setShowSlash(true);
      setShowAtPicker(false);
      ensureSlashSkills();
    } else {
      const range = detectAtMentionRange(val, cursor);
      if (!range) {
        setShowSlash(false);
        setShowAtPicker(false);
        return;
      }
      const query = range.token;
      if (query.length <= 120) {
        setShowSlash(false);
        setShowAtPicker(true);
        setAtQuery(query);
        setAtIndex(0);
        postMessage({ type: 'searchWorkspacePaths', query });
      } else {
        setShowAtPicker(false);
      }
    }
  }, [ensureSlashSkills]);

  const insertAtCursor = useCallback((value: string) => {
    const el = textareaRef.current;
    if (!el) return;
    const currentText = textRef.current;
    const start = el.selectionStart;
    const end = el.selectionEnd;
    const next = currentText.slice(0, start) + value + currentText.slice(end);
    updateText(next);
    // Exit history navigation mode (caller may have already reset, but guard here too)
    historyIndexRef.current = -1;
    requestAnimationFrame(() => {
      const current = textareaRef.current;
      if (!current) return;
      const pos = start + value.length;
      current.focus();
      current.setSelectionRange(pos, pos);
    });
  }, []);

  const addImageFiles = useCallback(async (files: File[] | FileList) => {
    const images = Array.from(files).filter((file) => file.type.startsWith('image/'));
    if (images.length === 0) return;
    const oversized = images.some((file) => file.size > MAX_IMAGE_BYTES);
    setAttachError(oversized ? t('input.imageTooLarge', { mb: MAX_IMAGE_MB }) : null);
    const allowed = images.filter((file) => file.size <= MAX_IMAGE_BYTES);
    if (allowed.length === 0) return;
    const parsed = (await Promise.all(allowed.map(fileToImageData))).filter(
      (img): img is ImageData => img !== null,
    );
    setPendingImages((prev) => [...prev, ...parsed].slice(0, MAX_IMAGES));
  }, [t]);

  const handleSend = useCallback(() => {
    if (state.approvalModePending) return;
    const value = textRef.current;
    const trimmed = value.trim();
    if (!trimmed && pendingImages.length === 0) return;
    send(trimmed, pendingImages.length > 0 ? pendingImages : undefined);
    updateText('');
    setPendingImages([]);
    setAttachError(null);
    setShowSlash(false);
    // Save to input history (skip duplicates of the last entry, cap at 100)
    const history = inputHistoryRef.current;
    if (trimmed && (history.length === 0 || history[history.length - 1] !== trimmed)) {
      inputHistoryRef.current = [...history, trimmed].slice(-MAX_INPUT_HISTORY);
    }
    historyIndexRef.current = -1;
  }, [pendingImages, send, state.approvalModePending]);

  const selectAtItem = useCallback((item: WorkspacePath) => {
    const el = textareaRef.current;
    if (!el) return;
    const range = detectAtMentionRange(text, el.selectionStart ?? text.length);
    if (!range) return;
    const next = applyAtMentionSelection(text, range, item.relativePath, item.isDir);
    updateText(next.text);
    historyIndexRef.current = -1;
    setShowAtPicker(next.keepOpen);
    setAtQuery(next.query);
    setAtIndex(0);
    if (next.keepOpen) {
      setAtItems([]);
      postMessage({ type: 'searchWorkspacePaths', query: next.query });
    } else {
      setAtItems([]);
    }
    requestAnimationFrame(() => {
      const current = textareaRef.current;
      if (!current) return;
      current.focus();
      current.setSelectionRange(next.cursor, next.cursor);
    });
  }, [text]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (showSlash) return;
      if (showAtPicker) {
        if (e.key === 'ArrowDown') {
          e.preventDefault();
          setAtIndex((index) => Math.min(index + 1, atItems.length - 1));
          return;
        }
        if (e.key === 'ArrowUp') {
          e.preventDefault();
          setAtIndex((index) => Math.max(index - 1, 0));
          return;
        }
        if ((e.key === 'Enter' || e.key === 'Tab') && atItems.length > 0) {
          e.preventDefault();
          selectAtItem(atItems[Math.min(atIndex, atItems.length - 1)]);
          return;
        }
        if (e.key === 'Escape') {
          setShowAtPicker(false);
          return;
        }
      }
      if (e.nativeEvent.isComposing || e.keyCode === 229) return;
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        handleSend();
        return;
      }

      // Arrow Up/Down: navigate input history
      const history = inputHistoryRef.current;
      if (history.length === 0) return;

      if (e.key === 'ArrowUp') {
        const el = textareaRef.current;
        if (!el) return;

        const cursorPos = el.selectionStart ?? 0;
        const onFirstLine = el.value.slice(0, cursorPos).indexOf('\n') === -1;

        if (!onFirstLine) {
          // Multi-line text: let browser move cursor up within the text
          return;
        }

        if (cursorPos !== 0) {
          // On first line but not at start: move cursor to beginning of line
          e.preventDefault();
          el.setSelectionRange(0, 0);
          return;
        }

        // Cursor at start of first line: navigate history
        e.preventDefault();
        // Save original text on first history navigation
        if (historyIndexRef.current === -1) {
          originalTextRef.current = textRef.current;
        }
        const cur = historyIndexRef.current;
        const newIndex = cur < history.length - 1 ? cur + 1 : history.length - 1;
        historyIndexRef.current = newIndex;
        updateText(history[history.length - 1 - newIndex]);
      } else if (e.key === 'ArrowDown') {
        // Don't intercept ArrowDown when not navigating history
        if (historyIndexRef.current < 0) return;
        const el = textareaRef.current;
        if (!el) return;
        // If cursor is not on the last line (has \n after it), let browser handle it
        if (el.value.slice(el.selectionEnd ?? el.value.length).indexOf('\n') !== -1) return;
        e.preventDefault();
        const newIndex = historyIndexRef.current - 1;
        historyIndexRef.current = newIndex;
        if (newIndex < 0) {
          updateText(originalTextRef.current);
        } else {
          updateText(history[history.length - 1 - newIndex]);
        }
      }
    },
    // history refs intentionally excluded from deps — refs don't trigger re-renders
    [atIndex, atItems, handleSend, selectAtItem, showAtPicker, showSlash],
  );

  const handleSlashSelect = useCallback((command: string) => {
    updateText(command + ' ');
    historyIndexRef.current = -1;
    setShowSlash(false);
    textareaRef.current?.focus();
}, []);

  const handleSlashButton = useCallback(() => {
    setShowFilePicker((fp) => {
      if (fp) { setFileQuery(''); return false; }
      return fp;
    });
    setShowSlash((open) => {
      if (open) { updateText(''); historyIndexRef.current = -1; return false; }
      updateText('/');
      historyIndexRef.current = -1;
      setSlashFilter('');
      ensureSlashSkills();
      return true;
    });
    textareaRef.current?.focus();
  }, [ensureSlashSkills]);

  // ── File picker ──────────────────────────────────────────────

  const handleAttachClick = useCallback(() => {
    setShowAttachMenu((prev) => !prev);
    setShowSlash(false);
    setShowFilePicker(false);
  }, []);

  const openFilePicker = useCallback(() => {
    setShowAttachMenu(false);
    setShowFilePicker(true);
    setShowSlash(false);
    postMessage({ type: 'searchWorkspaceFiles', query: '' });
  }, []);

  const pickPath = useCallback(() => {
    setShowAttachMenu(false);
    postMessage({ type: 'pickPathForInsert' });
  }, []);

  const pickContextFile = useCallback(() => {
    setShowAttachMenu(false);
    postMessage({ type: 'pickContextFile' });
  }, []);

  const openImagePicker = useCallback(() => {
    setShowAttachMenu(false);
    imageInputRef.current?.click();
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

  const handlePaste = useCallback((e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const item of Array.from(items)) {
      if (item.kind === 'file' && item.type.startsWith('image/')) {
        const file = item.getAsFile();
        if (file) files.push(file);
      }
    }
    if (files.length > 0) {
      e.preventDefault();
      void addImageFiles(files);
    }
  }, [addImageFiles]);

  const hasText = Boolean(text.trim() || pendingImages.length > 0);

  return (
    <div className="input-container" ref={containerRef}>
      {state.activeTodos.length > 0 && <TodoPanel items={state.activeTodos} />}
      <div className="input-box" ref={inputBoxRef}>
        {showSlash && (
          <SlashPicker
            filter={slashFilter}
            skills={slashSkills ?? []}
            onSelect={handleSlashSelect}
            onClose={() => setShowSlash(false)}
          />
        )}
        {showAtPicker && (
          <div className="file-picker at-mention-picker">
            <div className="file-picker-list" ref={atListRef}>
              {atItems.length === 0 ? (
                <div className="file-picker-empty">
                  {atQuery ? t('input.noMatchingFiles') : t('input.typeToSearchFiles')}
                </div>
              ) : (
                atItems.map((item, index) => (
                  <button
                    key={item.relativePath}
                    type="button"
                    className={`file-picker-item ${index === atIndex ? 'active' : ''}`}
                    onMouseEnter={() => setAtIndex(index)}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      selectAtItem(item);
                    }}
                  >
                    <span className="file-picker-item-icon">{item.isDir ? '📁' : '📄'}</span>
                    <span className="file-picker-item-body">
                      <span className="file-picker-item-name">{item.relativePath}</span>
                      <span className="file-picker-item-path">{item.isDir ? t('input.folder') : item.fileName}</span>
                    </span>
                  </button>
                ))
              )}
            </div>
          </div>
        )}
        {showAttachMenu && (
          <div className="attach-menu-popover" ref={attachMenuRef}>
            <button type="button" className="attach-menu-item" onClick={pickPath}>
              <span className="attach-menu-icon">#</span>
              <span>{t('input.insertPath')}</span>
            </button>
            <button type="button" className="attach-menu-item" onClick={pickContextFile}>
              <span className="attach-menu-icon">+</span>
              <span>{t('input.chooseFile')}</span>
            </button>
            <button type="button" className="attach-menu-item" onClick={openFilePicker}>
              <span className="attach-menu-icon">@</span>
              <span>{t('input.searchWorkspace')}</span>
            </button>
            <button type="button" className="attach-menu-item" onClick={openImagePicker}>
              <span className="attach-menu-icon">□</span>
              <span>{t('input.uploadImage')}</span>
            </button>
          </div>
        )}
        {showFilePicker && (
          <div className="file-picker">
            <input
              ref={fileSearchRef}
              className="file-picker-search"
              type="text"
              placeholder={t('input.searchProjectFiles')}
              value={fileQuery}
              onChange={(e) => handleFileSearch(e.target.value)}
            />
            <div className="file-picker-list">
              {workspaceFiles.length === 0 ? (
                <div className="file-picker-empty">
                  {fileQuery ? t('input.noMatchingFiles') : t('input.typeToSearchFiles')}
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
        {attachError && (
          <div className="input-attach-error" role="alert">
            <span>{attachError}</span>
            <button type="button" onClick={() => setAttachError(null)} aria-label={t('input.dismiss')}>×</button>
          </div>
        )}
        {pendingImages.length > 0 && (
          <div className="input-image-previews">
            {pendingImages.map((img, index) => (
              <div className="input-image-preview" key={`${img.media_type}-${index}`}>
                <img src={imageDataUrl(img)} alt="" />
                <button
                  type="button"
                  aria-label={t('input.removeImage')}
                  title={t('input.removeImage')}
                  onClick={() => setPendingImages((prev) => prev.filter((_, i) => i !== index))}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
        )}
        {state.contextFiles.length > 0 && (
          <div className="attached-files">
            {state.contextFiles.map((f) => (
              <span
                key={f.path + (f.startLine || '')}
                className={`attached-file-pill ${f.type === 'selection' ? 'clickable' : ''}`}
                title={f.type === 'selection' && f.startLine
                  ? `${f.path}:${f.startLine}-${f.endLine}`
                  : f.path
                }
                onClick={f.type === 'selection' ? () => postMessage({ type: 'openFile', path: f.path, startLine: f.startLine, endLine: f.endLine }) : undefined}
              >
                <span className="pill-icon">{f.type === 'selection' ? '📋' : '📄'}</span>
                <span className="pill-name">
                  {f.type === 'selection' && f.startLine
                    ? `${f.fileName}:${f.startLine}-${f.endLine}`
                    : f.fileName
                  }
                </span>
                <button className="pill-close" onClick={(e) => { e.stopPropagation(); dispatch({ type: 'REMOVE_CONTEXT_FILE', path: f.path, startLine: f.startLine }); }}>×</button>
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
          onPaste={handlePaste}
          placeholder={t('input.placeholder')}
          rows={1}
        />
        <input
          ref={imageInputRef}
          className="hidden-file-input"
          type="file"
          accept="image/*"
          multiple
          onChange={(e) => {
            if (e.currentTarget.files) void addImageFiles(e.currentTarget.files);
            e.currentTarget.value = '';
          }}
        />
        <div className="input-footer">
          <button className="footer-slash-btn" onClick={handleSlashButton} title={t('input.commands')}>
            /
          </button>
          <button className="footer-attach-btn" onClick={handleAttachClick} title={t('input.attachFile')}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
            </svg>
          </button>
          <span className="footer-spacer" />
          {state.tokenCount && <span className="footer-tokens">{formatTokenCount(state.tokenCount.total, t)}</span>}
          <ModeSelector placement="up" onOpen={() => setShowSlash(false)} />
          <ModelSelector placement="up" onOpen={() => setShowSlash(false)} />
          {state.isGenerating || state.recoveryLocked ? (
            <>
              {state.isGenerating && !state.recoveryLocked && hasText && (
                <button className="btn-send" onClick={handleSend} disabled={state.approvalModePending} title={t('input.queueMessage')}>
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
                    <line x1="12" y1="19" x2="12" y2="5" /><polyline points="5 12 12 5 19 12" />
                  </svg>
                </button>
              )}
              <button className="btn-stop" onClick={stop} title={t('input.stop')}>
                <div style={{ width: 8, height: 8, background: 'currentColor', borderRadius: 1 }} />
              </button>
            </>
          ) : (
            <button className="btn-send" onClick={handleSend} disabled={state.approvalModePending || !hasText} title={t('input.send')}>
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
