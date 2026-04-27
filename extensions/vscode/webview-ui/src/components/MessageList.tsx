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
    <>
      <div className="messages-container">
        {state.messages.map((msg) => {
          if (msg.role === 'user') return <UserMessage key={msg.id} message={msg} />;
          if (msg.role === 'assistant') return <AssistantMessage key={msg.id} message={msg} />;
          if (msg.role === 'error') {
            return (
              <div key={msg.id} className="timeline-message dot-error">
                <div className="error-message-content">{msg.text}</div>
              </div>
            );
          }
          return null;
        })}
        <div ref={bottomRef} />
      </div>
      <div className="message-gradient" />
    </>
  );
}
