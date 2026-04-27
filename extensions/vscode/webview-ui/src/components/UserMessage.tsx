import React from 'react';
import { ChatMessage } from '../state/types';

interface UserMessageProps {
  message: ChatMessage;
  className?: string;
}

export function UserMessage({ message, className = '' }: UserMessageProps) {
  return (
    <div className={`timeline-message user-message-wrapper dot-brand${className}`}>
      <div className="user-message-bubble">{message.text}</div>
    </div>
  );
}
