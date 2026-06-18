import React from 'react';
import { Markdown } from './Markdown';
import { ToolCall } from './ToolCall';
import { ArtifactViewer } from './ArtifactViewer';
import { PermissionInline } from './PermissionInline';
import { ReasoningBlock } from './ReasoningBlock';
import type { MessageViewModel, PermissionViewModel } from '../state/types';

interface Props {
  message: MessageViewModel;
  pendingPermission: PermissionViewModel | null;
}

export const AssistantMessage: React.FC<Props> = ({ message, pendingPermission }) => (
  <div className="assistant-message">
    <div className="content">
      {message.reasoning && <ReasoningBlock text={message.reasoning} />}
      {message.text && <Markdown content={message.text} />}
      {message.status === 'streaming' && <span className="streaming-cursor">▊</span>}
    </div>
    {message.tool_calls.map(tc => <ToolCall key={tc.call_id} tool={tc} />)}
    {message.artifacts.map(a => <ArtifactViewer key={a.id} artifact={a} />)}
    {pendingPermission && <PermissionInline permission={pendingPermission} />}
  </div>
);
