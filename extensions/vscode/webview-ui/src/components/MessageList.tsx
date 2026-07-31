import React, { useRef, useEffect, useCallback, useState, useMemo } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { UserMessage } from './UserMessage';
import { AssistantMessage } from './AssistantMessage';
import { SearchBar } from './SearchBar';
import { useT } from '../i18n';
import { highlightPlainText } from '../utils/search';

export function MessageList() {
  const { state } = useChatContext();
  const t = useT();
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
  // Use instant scroll to avoid layout repaint flicker during frequent
  // tool-result updates (bash commands with long output). Smooth scroll
  // is reserved for the manual scroll-to-bottom button click.
  useEffect(() => {
    if (!isUserScrolledUp) {
      bottomRef.current?.scrollIntoView();
    }
  }, [state.messages, state.queuedMessages, state.isGenerating]);

  const query = state.searchQuery;
  const hasSearch = query.trim().length > 0;
  const lastMessageId = state.messages[state.messages.length - 1]?.id;

  // Build a set of message ids that contain matches, plus the id of the
  // message that holds the currently focused match.
  const matchedIds = useMemo(() => {
    if (!hasSearch) return new Set<string>();
    return new Set(state.search.matches.map((m) => m.messageId));
  }, [hasSearch, state.search.matches]);

  const currentMatchMessageId = useMemo(() => {
    if (!hasSearch || state.search.matches.length === 0) return undefined;
    return state.search.matches[state.search.currentMatchIndex]?.messageId;
  }, [hasSearch, state.search]);

  return (
    <>
      <SearchBar />
      <div
        ref={containerRef}
        className={`messages-container${hasSearch ? ' dimmed' : ''}`}
        onScroll={handleScroll}
      >
        {state.messages.map((msg) => {
          const isMatch = hasSearch && matchedIds.has(msg.id);
          const isCurrentMatch = msg.id === currentMatchMessageId;
          const highlightClass = `${isMatch ? ' highlighted' : ''}${msg.id === lastMessageId ? ' is-last' : ''}`;

          if (msg.role === 'user') {
            return (
              <UserMessage
                key={msg.id}
                message={msg}
                className={highlightClass}
                searchQuery={hasSearch ? query : undefined}
                isCurrentMatch={isCurrentMatch}
              />
            );
          }
          if (msg.role === 'assistant') {
            return (
              <AssistantMessage
                key={msg.id}
                message={msg}
                className={highlightClass}
                searchQuery={hasSearch ? query : undefined}
                isCurrentMatch={isCurrentMatch}
              />
            );
          }
          if (msg.role === 'error') {
            return (
              <div key={msg.id} className={`timeline-message dot-error${highlightClass}`}>
                <div className="error-message-content">
                  {hasSearch
                    ? <span dangerouslySetInnerHTML={{ __html: highlightPlainText(msg.text ?? '', query) }} />
                    : msg.text}
                </div>
              </div>
            );
          }
          return null;
        })}
        {state.queuedMessages.map((msg) => {
          const isMatch = hasSearch && matchedIds.has(msg.id);
          const highlightClass = isMatch ? ' highlighted' : '';
          return (
            <UserMessage
              key={msg.id}
              message={msg}
              className={highlightClass}
              searchQuery={hasSearch ? query : undefined}
              isCurrentMatch={false}
            />
          );
        })}
        <div ref={bottomRef} />
        {isUserScrolledUp && (
          <button
            className="scroll-to-bottom-btn"
            onClick={scrollToBottom}
            aria-label={t('search.scrollLatest')}
            title={t('search.scrollLatest')}
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
