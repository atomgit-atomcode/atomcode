import React, { useState, useCallback } from 'react';
import { ArtifactData, ChatMessage, MessageBlock } from '../state/types';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';
import { PermissionRequest } from './PermissionRequest';
import { ArtifactCodeView } from './ArtifactCodeView';
import { blocksFromLegacyMessage } from '../state/blocks';
import { classifyArtifactRenderKind, normalizeMarkdownArtifactContent, shouldRenderArtifactChrome } from './artifactRendering';
import { useT } from '../i18n';

interface AssistantMessageProps {
  message: ChatMessage;
  className?: string;
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

function blockCopyText(blocks: MessageBlock[]): string {
  return blocks.map((block) => {
    if (block.type === 'text') return block.content;
    if (block.type === 'artifact') return block.artifact.content;
    return '';
  }).filter(Boolean).join('\n\n');
}

function AssistantBlock({ block, streaming }: { block: MessageBlock; streaming: boolean }) {
  switch (block.type) {
    case 'text':
      return block.content ? <Markdown content={block.content} streaming={streaming} /> : null;
    case 'tool':
      return <ToolCall tool={block.tool} />;
    case 'artifact':
      if (classifyArtifactRenderKind(block.artifact) === 'markdown') {
        return block.artifact.content
          ? <Markdown content={normalizeMarkdownArtifactContent(block.artifact.content)} streaming={block.artifact.status === 'streaming'} />
          : null;
      }
      return shouldRenderArtifactChrome(block.artifact)
        ? <ArtifactBlock artifact={block.artifact} />
        : <ArtifactCodeView artifact={block.artifact} />;
    case 'permission':
      return block.request.status === 'pending' ? <PermissionRequest request={block.request} /> : null;
    default:
      return null;
  }
}

function getDotClass(isStreaming: boolean, hasError: boolean): string {
  if (isStreaming) return 'dot-brand dot-blink';
  if (hasError) return 'dot-error';
  return 'dot-success';
}

export function AssistantMessage({ message, className = '' }: AssistantMessageProps) {
  const t = useT();
  const blocks = message.blocks && message.blocks.length > 0 ? message.blocks : blocksFromLegacyMessage(message);
  const hasError = blocks.some((block) => block.type === 'tool' && block.tool.status === 'error')
    || message.toolCalls?.some((t) => t.status === 'error');
  const isStreaming = message.streaming;
  const dotClass = getDotClass(isStreaming, hasError);
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    const text = blockCopyText(blocks);
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [blocks]);

  const hasContent = blocks.length > 0;

  return (
    <div className={`timeline-message ${dotClass}${className}`}>
      <div className="assistant-message-content">
        <div className="assistant-block-list">
          {blocks.map((block) => <AssistantBlock key={block.id} block={block} streaming={isStreaming} />)}
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
