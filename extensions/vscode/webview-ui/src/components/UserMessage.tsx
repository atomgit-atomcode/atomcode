import React from 'react';
import { ChatMessage } from '../state/types';

interface UserMessageProps {
  message: ChatMessage;
}

export function UserMessage({ message }: UserMessageProps) {
  return (
    <div className="timeline-message user-message-wrapper dot-brand">
      <div className="user-message-bubble">{message.text}</div>
    </div>
  );
}
