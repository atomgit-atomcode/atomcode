import React from 'react';
import { ChatMessage } from '../state/types';

interface UserMessageProps {
  message: ChatMessage;
}

export function UserMessage({ message }: UserMessageProps) {
  return (
    <div className="message user-message">
      <div className="message-role">You</div>
      <div className="message-text">{message.text}</div>
    </div>
  );
}
