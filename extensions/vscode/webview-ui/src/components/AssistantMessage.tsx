import React from 'react';
import { ChatMessage } from '../state/types';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';

interface AssistantMessageProps {
  message: ChatMessage;
}

export function AssistantMessage({ message }: AssistantMessageProps) {
  const hasError = message.toolCalls?.some((t) => t.status === 'error');
  const isStreaming = message.streaming;
  const dotClass = isStreaming ? 'dot-brand dot-blink' : hasError ? 'dot-error' : 'dot-success';

  return (
    <div className={`timeline-message ${dotClass}`}>
      <div className="assistant-message-content">
        {message.text && <Markdown content={message.text} />}
        {isStreaming && !message.text && <span className="streaming-cursor" />}
        {message.toolCalls && message.toolCalls.length > 0 &&
          message.toolCalls.map((tool) => <ToolCall key={tool.id} tool={tool} />)
        }
        {isStreaming && message.text && <span className="streaming-cursor" />}
      </div>
    </div>
  );
}
