import React, { useState, useCallback, useRef, useEffect } from 'react';
import { ArtifactData, ChatMessage, MessageBlock, StatusData } from '../state/types';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';
import { PermissionRequest } from './PermissionRequest';
import { ArtifactCodeView } from './ArtifactCodeView';
import { blocksFromLegacyMessage } from '../state/blocks';
import { shouldRenderToolCall } from '../state/todo';
import { classifyArtifactRenderKind, normalizeMarkdownArtifactContent, shouldRenderArtifactChrome } from './artifactRendering';
import { useT } from '../i18n';

interface AssistantMessageProps {
  message: ChatMessage;
  className?: string;
  searchQuery?: string;
  isCurrentMatch?: boolean;
}

function ArtifactBlock({ artifact }: { artifact: ArtifactData }) {
  const t = useT();
  const label = artifact.title || artifact.language || artifact.artifactType || t('assistant.artifact');
  const isStreaming = artifact.status === 'streaming';

  return (
    <div className={`artifact-block${isStreaming ? ' is-streaming' : ''}`}>
      <div className="artifact-header">
        <span className="artifact-title">{label}</span>
        {artifact.language && <span className="artifact-meta">{artifact.language}</span>}
        {isStreaming && <span className="artifact-status">{t('assistant.streaming')}</span>}
      </div>
      <ArtifactCodeView artifact={artifact} />
    </div>
  );
}

function StatusBlock({ status }: { status: StatusData }) {
  const t = useT();
  const message = status.kind === 'rate_limited'
    ? status.retryAfterSeconds !== undefined
      ? t('stream.rateLimitedRetrying', { seconds: status.retryAfterSeconds })
      : t('stream.rateLimitedPaused')
    : status.message;
  const attempt = status.kind === 'rate_limited' && status.attempt && status.maxAttempts
    ? `${status.attempt}/${status.maxAttempts}`
    : undefined;

  return (
    <div className={`assistant-status assistant-status-${status.kind}`}>
      <span className="assistant-status-message">{message}</span>
      {attempt && <span className="assistant-status-meta">{attempt}</span>}
    </div>
  );
}

function blockCopyText(blocks: MessageBlock[]): string {
  return blocks.map((block) => {
    if (block.type === 'text') return block.content;
    if (block.type === 'artifact') return block.artifact.content;
    if (block.type === 'status') return block.status.message;
    return '';
  }).filter(Boolean).join('\n\n');
}

function AssistantBlock({ block, streaming, searchQuery }: { block: MessageBlock; streaming: boolean; searchQuery?: string }) {
  switch (block.type) {
    case 'text':
      return block.content ? <Markdown content={block.content} streaming={streaming} searchQuery={searchQuery} /> : null;
    case 'tool':
      return <ToolCall tool={block.tool} />;
    case 'artifact':
      if (classifyArtifactRenderKind(block.artifact) === 'markdown') {
        return block.artifact.content
          ? <Markdown content={normalizeMarkdownArtifactContent(block.artifact.content)} streaming={block.artifact.status === 'streaming'} searchQuery={searchQuery} />
          : null;
      }
      return shouldRenderArtifactChrome(block.artifact)
        ? <ArtifactBlock artifact={block.artifact} />
        : <ArtifactCodeView artifact={block.artifact} />;
    case 'permission':
      return block.request.status === 'pending' || block.request.status === 'submitting'
        ? <PermissionRequest request={block.request} />
        : null;
    case 'status':
      return <StatusBlock status={block.status} />;
    default:
      return null;
  }
}

function getDotClass(isStreaming: boolean, hasError: boolean): string {
  if (isStreaming) return 'dot-brand dot-blink';
  if (hasError) return 'dot-error';
  return 'dot-success';
}

export function AssistantMessage({ message, className = '', searchQuery, isCurrentMatch }: AssistantMessageProps) {
  const t = useT();
  const contentRef = useRef<HTMLDivElement>(null);
  const allBlocks = message.blocks && message.blocks.length > 0 ? message.blocks : blocksFromLegacyMessage(message);
  const blocks = allBlocks.filter((block) => block.type !== 'tool' || shouldRenderToolCall(block.tool));
  const hasError = blocks.some((block) =>
    block.type === 'tool' && (block.tool.status === 'error' || block.tool.status === 'incomplete'));
  const isStreaming = Boolean(message.streaming);
  const dotClass = getDotClass(isStreaming, hasError);
  const [copied, setCopied] = useState(false);

  // Scroll the active match into view.
  useEffect(() => {
    if (!isCurrentMatch) return;
    const el = contentRef.current;
    if (!el) return;
    const mark = el.querySelector('mark.search-highlight');
    if (mark) {
      mark.scrollIntoView({ behavior: 'smooth', block: 'center' });
    } else {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    }
  }, [isCurrentMatch, searchQuery]);

  const handleCopy = useCallback(() => {
    const text = blockCopyText(blocks);
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [blocks]);

  const hasContent = blocks.length > 0;
  const onlyHiddenTodoBlocks = allBlocks.length > 0 && blocks.length === 0;

  if (onlyHiddenTodoBlocks) return null;

  return (
    <div className={`timeline-message ${dotClass}${className}${isCurrentMatch ? ' search-current' : ''}`}>
      <div className="assistant-message-content" ref={contentRef}>
        <div className="assistant-block-list">
          {blocks.map((block) => <AssistantBlock key={block.id} block={block} streaming={isStreaming} searchQuery={searchQuery} />)}
        </div>
        {isStreaming && !hasContent && <span className="streaming-cursor" />}
        {isStreaming && hasContent && <span className="streaming-cursor" />}
        {hasContent && !isStreaming && (
          <button className="msg-copy-btn" onClick={handleCopy}>
            {copied ? `✓ ${t('assistant.copied')}` : t('assistant.copy')}
          </button>
        )}
      </div>
    </div>
  );
}
