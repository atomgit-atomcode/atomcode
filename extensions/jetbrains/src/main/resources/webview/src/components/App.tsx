import React from 'react';
import { MessageList } from './MessageList';
import type { ChatViewModel } from '../state/types';

interface Props {
  model: ChatViewModel | null;
}

export const App: React.FC<Props> = ({ model }) => {
  if (!model) {
    return <div className="app"><div className="loading">Loading...</div></div>;
  }

  return (
    <div className="app">
      <MessageList
        messages={model.messages}
        isGenerating={model.is_generating}
        pendingPermission={model.pending_permission}
        generationError={model.generation_error}
      />
    </div>
  );
};
