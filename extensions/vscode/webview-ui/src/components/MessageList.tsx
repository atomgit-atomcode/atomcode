import React, { useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { UserMessage } from './UserMessage';
import { AssistantMessage } from './AssistantMessage';
import { SearchBar } from './SearchBar';

export function MessageList() {
  const { state } = useChatContext();
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [state.messages, state.queuedMessages, state.isGenerating]);

  const query = state.searchQuery.toLowerCase();
  const hasSearch = query.length > 0;
  const lastMessageId = state.messages[state.messages.length - 1]?.id;

  return (
    <>
      <SearchBar />
      <div className={`messages-container${hasSearch ? ' dimmed' : ''}`}>
        {state.messages.map((msg) => {
          const matches = hasSearch && msg.text.toLowerCase().includes(query);
          const highlightClass = `${matches ? ' highlighted' : ''}${msg.id === lastMessageId ? ' is-last' : ''}`;

          if (msg.role === 'user') return <UserMessage key={msg.id} message={msg} className={highlightClass} />;
          if (msg.role === 'assistant') return <AssistantMessage key={msg.id} message={msg} className={highlightClass} />;
          if (msg.role === 'error') {
            return (
              <div key={msg.id} className={`timeline-message dot-error${highlightClass}`}>
                <div className="error-message-content">{msg.text}</div>
              </div>
            );
          }
          return null;
        })}
        {state.queuedMessages.map((msg) => {
          const matches = hasSearch && msg.text.toLowerCase().includes(query);
          const highlightClass = matches ? ' highlighted' : '';
          return <UserMessage key={msg.id} message={msg} className={highlightClass} />;
        })}
        <div ref={bottomRef} />
      </div>
      <div className="message-gradient" />
    </>
  );
}
