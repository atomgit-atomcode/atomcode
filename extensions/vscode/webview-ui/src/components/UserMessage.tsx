import React from 'react';
import { ChatMessage } from '../state/types';

interface UserMessageProps {
  message: ChatMessage;
  className?: string;
}

export function UserMessage({ message, className = '' }: UserMessageProps) {
  return (
    <div className={`user-message-wrapper${className}`}>
      <div className="user-message-bubble">
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
        <div>{message.text}</div>
      </div>
    </div>
  );
}
