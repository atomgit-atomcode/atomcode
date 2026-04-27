import React from 'react';
import { ChatMessage } from '../state/types';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';

interface AssistantMessageProps {
  message: ChatMessage;
}

export function AssistantMessage({ message }: AssistantMessageProps) {
  return (
    <div className="message assistant-message">
      <div className="message-role">AtomCode</div>
      {message.text && <Markdown content={message.text} />}
      {message.streaming && !message.text && (
        <span className="streaming-cursor" />
      )}
      {message.toolCalls && message.toolCalls.length > 0 && (
        <div className="tool-calls-list">
          {message.toolCalls.map((tool) => (
            <ToolCall key={tool.id} tool={tool} />
          ))}
        </div>
      )}
      {message.streaming && message.text && (
        <span className="streaming-cursor" />
      )}
    </div>
  );
}
