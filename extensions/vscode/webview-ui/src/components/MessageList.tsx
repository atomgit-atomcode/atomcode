import React, { useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { UserMessage } from './UserMessage';
import { AssistantMessage } from './AssistantMessage';

export function MessageList() {
  const { state } = useChatContext();
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [state.messages, state.isGenerating]);

  return (
    <div className="message-list">
      {state.messages.map((msg) => {
        if (msg.role === 'user') {
          return <UserMessage key={msg.id} message={msg} />;
        }
        if (msg.role === 'assistant') {
          return <AssistantMessage key={msg.id} message={msg} />;
        }
        if (msg.role === 'error') {
          return (
            <div key={msg.id} className="message error-message">
              <div className="message-role">Error</div>
              <div className="message-text">{msg.text}</div>
            </div>
          );
        }
        return null;
      })}
      <div ref={bottomRef} />
    </div>
  );
}
