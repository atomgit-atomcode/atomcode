import React, { useRef, useEffect, useCallback, useState } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { UserMessage } from './UserMessage';
import { AssistantMessage } from './AssistantMessage';
import { SearchBar } from './SearchBar';

export function MessageList() {
  const { state } = useChatContext();
  const bottomRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [isUserScrolledUp, setIsUserScrolledUp] = useState(false);

  const scrollToBottom = useCallback(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
    setIsUserScrolledUp(false);
  }, []);

  // Detect whether the user has scrolled away from the bottom
  const handleScroll = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    setIsUserScrolledUp(!atBottom);
  }, []);

  // Only auto-scroll if the user hasn't scrolled up
  useEffect(() => {
    if (!isUserScrolledUp) {
      bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
    }
  }, [state.messages, state.queuedMessages, state.isGenerating]);

  const query = state.searchQuery.toLowerCase();
  const hasSearch = query.length > 0;
  const lastMessageId = state.messages[state.messages.length - 1]?.id;

  return (
    <>
      <SearchBar />
      <div
        ref={containerRef}
        className={`messages-container${hasSearch ? ' dimmed' : ''}`}
        onScroll={handleScroll}
      >
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
        {isUserScrolledUp && (
          <button
            className="scroll-to-bottom-btn"
            onClick={scrollToBottom}
            aria-label="Scroll to latest"
            title="Scroll to latest"
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
              <path d="M4 6L8 10L12 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
        )}
      </div>
      <div className="message-gradient" />
    </>
  );
}
