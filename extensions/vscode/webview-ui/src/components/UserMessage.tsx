import React, { useMemo, useState } from 'react';
import { ChatMessage } from '../state/types';

interface UserMessageProps {
  message: ChatMessage;
  className?: string;
}

export function UserMessage({ message, className = '' }: UserMessageProps) {
  const [expanded, setExpanded] = useState(false);
  const shouldCollapse = useMemo(() => {
    const lineCount = message.text.split('\n').length;
    return message.text.length > 1200 || lineCount > 18;
  }, [message.text]);

  return (
    <div className={`user-message-wrapper${message.queued ? ' is-queued' : ''}${className}`}>
      <div className="user-message-bubble">
        {message.queued && <div className="user-message-status">Queued</div>}
        {message.contextFiles && message.contextFiles.length > 0 && (
          <div className="user-message-attachments">
            {message.contextFiles.map((file) => (
              <span key={file.path} className="user-message-attachment" title={file.path}>
                <span className="user-message-attachment-icon">{file.type === 'selection' ? 'Selection' : 'File'}</span>
                <span className="user-message-attachment-name">{file.fileName}</span>
              </span>
            ))}
          </div>
        )}
        <div className={`user-message-text${shouldCollapse && !expanded ? ' is-collapsed' : ''}`}>
          {message.text}
        </div>
        {shouldCollapse && (
          <button
            type="button"
            className="user-message-toggle"
            onClick={() => setExpanded((value) => !value)}
          >
            {expanded ? 'Collapse' : 'Expand'}
          </button>
        )}
      </div>
    </div>
  );
}
