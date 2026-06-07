import React, { useState, useCallback } from 'react';
import { ChatMessage } from '../state/types';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';
import { PermissionRequest } from './PermissionRequest';

interface AssistantMessageProps {
  message: ChatMessage;
  className?: string;
}

export function AssistantMessage({ message, className = '' }: AssistantMessageProps) {
  const hasError = message.toolCalls?.some((t) => t.status === 'error');
  const isStreaming = message.streaming;
  const dotClass = isStreaming ? 'dot-brand dot-blink' : hasError ? 'dot-error' : 'dot-success';
  const toolCalls = message.toolCalls ?? [];
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(message.text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [message.text]);

  const hasContent = toolCalls.length > 0 || message.text;

  return (
    <div className={`timeline-message ${dotClass}${className}`}>
      <div className="assistant-message-content">
        {toolCalls.length > 0 && (
          <div className="tool-list">
            {toolCalls.map((tool) => <ToolCall key={tool.id} tool={tool} />)}
          </div>
        )}
        {message.text && <Markdown content={message.text} />}
        {isStreaming && !message.text && <span className="streaming-cursor" />}
        {message.permissionRequest && message.permissionRequest.status === 'pending' && (
          <PermissionRequest request={message.permissionRequest} />
        )}
        {isStreaming && message.text && <span className="streaming-cursor" />}
        {hasContent && !isStreaming && (
          <button className="msg-copy-btn" onClick={handleCopy}>
            {copied ? '✓ Copied' : 'Copy'}
          </button>
        )}
      </div>
    </div>
  );
}
