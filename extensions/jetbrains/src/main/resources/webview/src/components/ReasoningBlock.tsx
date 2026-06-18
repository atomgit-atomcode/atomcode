import React, { useState } from 'react';

interface Props { text: string }

export const ReasoningBlock: React.FC<Props> = ({ text }) => {
  const [open, setOpen] = useState(false);
  return (
    <div className="reasoning-block">
      <div className="reasoning-header" onClick={() => setOpen(!open)}>
        💭 Reasoning {open ? '▾' : '▸'}
      </div>
      {open && <div className="reasoning-content">{text}</div>}
    </div>
  );
};
