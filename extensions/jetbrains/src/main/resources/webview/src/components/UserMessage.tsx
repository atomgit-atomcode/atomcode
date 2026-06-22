import React from 'react';
import type { MessageViewModel } from '../state/types';

interface Props { message: MessageViewModel }

export const UserMessage: React.FC<Props> = ({ message }) => (
  <div className="user-message">
    <div className="bubble">
      {message.context_files.length > 0 && (
        <div className="context-chips">
          {message.context_files.map(f => <span key={f} className="chip">{f}</span>)}
        </div>
      )}
      <div className="text">{message.text}</div>
      {message.images.map((img, i) => (
        <img key={i} src={`data:${img.media_type};base64,${img.data}`} className="attached-image" alt="attachment" />
      ))}
      {message.queued && <span className="queued-badge">queued</span>}
    </div>
  </div>
);
