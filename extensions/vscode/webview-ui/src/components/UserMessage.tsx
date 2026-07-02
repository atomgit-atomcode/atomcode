import React, { useMemo, useState } from 'react';
import { ChatMessage } from '../state/types';
import { Markdown } from './Markdown';
import { useT } from '../i18n';

interface UserMessageProps {
  message: ChatMessage;
  className?: string;
}

export function UserMessage({ message, className = '' }: UserMessageProps) {
  const [expanded, setExpanded] = useState(false);
  const t = useT();
  const shouldCollapse = useMemo(() => {
    const lineCount = message.text.split('\n').length;
    return message.text.length > 1200 || lineCount > 18;
  }, [message.text]);

  return (
    <div className={`user-message-wrapper${message.queued ? ' is-queued' : ''}${className}`}>
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
          <Markdown content={message.text} />
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
