import React, { useRef, useEffect } from 'react';
import { UserMessage } from './UserMessage';
import { AssistantMessage } from './AssistantMessage';
import { ThinkingIndicator } from './ThinkingIndicator';
import type { MessageViewModel, PermissionViewModel } from '../state/types';

interface Props {
  messages: MessageViewModel[];
  isGenerating: boolean;
  pendingPermission: PermissionViewModel | null;
  generationError: string | null;
}

export const MessageList: React.FC<Props> = ({ messages, isGenerating, pendingPermission, generationError }) => {
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages.length]);

  return (
    <div className="message-list">
      {messages.map(msg =>
        msg.role === 'user'
          ? <UserMessage key={msg.id} message={msg} />
          : <AssistantMessage key={msg.id} message={msg} pendingPermission={pendingPermission} />
      )}
      {isGenerating && messages.length > 0 && messages[messages.length - 1]?.role === 'user' && (
        <ThinkingIndicator />
      )}
      {generationError && <div className="error-block">Error: {generationError}</div>}
      <div ref={bottomRef} />
    </div>
  );
};
