import React, { useMemo, useState, useEffect, useRef } from 'react';
import { ChatMessage } from '../state/types';
import { useT } from '../i18n';
import { highlightPlainText } from '../utils/search';

interface UserMessageProps {
  message: ChatMessage;
  className?: string;
  searchQuery?: string;
  isCurrentMatch?: boolean;
}

export function UserMessage({ message, className = '', searchQuery, isCurrentMatch }: UserMessageProps) {
  const [expanded, setExpanded] = useState(false);
  const t = useT();
  const textRef = useRef<HTMLDivElement>(null);
  const shouldCollapse = useMemo(() => {
    const lineCount = message.text.split('\n').length;
    return message.text.length > 1200 || lineCount > 18;
  }, [message.text]);

  // Auto-expand when this message is the current search match, so the
  // highlighted keyword is visible even in collapsed bubbles.
  useEffect(() => {
    if (isCurrentMatch && shouldCollapse) {
      setExpanded(true);
    }
  }, [isCurrentMatch, shouldCollapse]);

  // Scroll the active match into view. Deferred to the next frame so the
  // expand state has flushed to the DOM (overflow: hidden can otherwise
  // prevent the keyword from being scrolled into the visible area).
  useEffect(() => {
    if (!isCurrentMatch) return;
    const raf = requestAnimationFrame(() => {
      const el = textRef.current;
      if (!el) return;
      const mark = el.querySelector('mark.search-highlight');
      if (mark) {
        mark.scrollIntoView({ behavior: 'smooth', block: 'center' });
      } else {
        el.scrollIntoView({ behavior: 'smooth', block: 'center' });
      }
    });
    return () => cancelAnimationFrame(raf);
  }, [isCurrentMatch, searchQuery, expanded]);

  const textHtml = useMemo(
    () => (searchQuery && searchQuery.trim() ? highlightPlainText(message.text, searchQuery) : undefined),
    [message.text, searchQuery],
  );

  return (
    <div className={`user-message-wrapper${message.queued ? ' is-queued' : ''}${className}${isCurrentMatch ? ' search-current' : ''}`}>
      <div className="user-message-bubble">
        {message.queued && <div className="user-message-status">{t('user.queued')}</div>}
        {message.contextFiles && message.contextFiles.length > 0 && (
          <div className="user-message-attachments">
            {message.contextFiles.map((file) => (
              <span key={file.path} className="user-message-attachment" title={file.path}>
                <span className="user-message-attachment-icon">{file.type === 'selection' ? t('user.selection') : t('user.file')}</span>
                <span className="user-message-attachment-name">{file.fileName}</span>
              </span>
            ))}
          </div>
        )}
        {message.images && message.images.length > 0 && (
          <div className="user-message-images">
            {message.images.map((img, index) => (
              img.missing || !img.data ? (
                <div
                  key={`${img.media_type}-${index}`}
                  className="user-message-image-placeholder"
                  role="img"
                  aria-label={t('user.imageUnavailable')}
                  title={t('user.imageUnavailable')}
                >
                  <span aria-hidden="true" className="user-message-image-placeholder-icon">▧</span>
                  <span>{t('user.imageUnavailable')}</span>
                </div>
              ) : (
                <img
                  key={`${img.media_type}-${index}`}
                  className="user-message-image"
                  src={`data:${img.media_type};base64,${img.data}`}
                  alt=""
                />
              )
            ))}
          </div>
        )}
        <div className={`user-message-text${shouldCollapse && !expanded ? ' is-collapsed' : ''}`}>
          <div ref={textRef} className="user-message-plain-text">
            {textHtml
              ? <span dangerouslySetInnerHTML={{ __html: textHtml }} />
              : message.text}
          </div>
        </div>
        {shouldCollapse && (
          <button
            type="button"
            className="user-message-toggle"
            onClick={() => setExpanded((value) => !value)}
          >
            {expanded ? t('user.collapse') : t('user.expand')}
          </button>
        )}
      </div>
    </div>
  );
}
