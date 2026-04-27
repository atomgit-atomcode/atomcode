import React from 'react';

interface DiffLine {
  type: 'add' | 'del' | 'ctx';
  oldNum?: number;
  newNum?: number;
  text: string;
}

interface DiffViewProps {
  content: string;
}

function parseDiff(raw: string): DiffLine[] {
  const lines: DiffLine[] = [];
  let oldNum = 0;
  let newNum = 0;
  for (const line of raw.split('\n')) {
    if (line.startsWith('@@')) {
      const m = line.match(/@@ -(\d+)/);
      if (m) { oldNum = parseInt(m[1], 10); newNum = oldNum; }
      continue;
    }
    if (line.startsWith('-')) {
      lines.push({ type: 'del', oldNum: oldNum++, text: line.slice(1) });
    } else if (line.startsWith('+')) {
      lines.push({ type: 'add', newNum: newNum++, text: line.slice(1) });
    } else {
      lines.push({ type: 'ctx', oldNum: oldNum++, newNum: newNum++, text: line.startsWith(' ') ? line.slice(1) : line });
    }
  }
  return lines;
}

export function DiffView({ content }: DiffViewProps) {
  const lines = parseDiff(content);
  if (lines.length === 0) return null;

  return (
    <div style={{
      fontFamily: 'var(--app-monospace-font-family)',
      fontSize: 'var(--app-monospace-font-size-small)',
      overflowX: 'auto',
    }}>
      <div style={{ display: 'grid', gridTemplateColumns: '32px 32px 1fr' }}>
        {lines.map((line, i) => (
          <React.Fragment key={i}>
            <div style={{
              padding: '1px 4px', textAlign: 'right', userSelect: 'none',
              color: line.type === 'del' ? '#c74e3988' : '#555',
            }}>
              {line.oldNum ?? ''}
            </div>
            <div style={{
              padding: '1px 4px', textAlign: 'right', userSelect: 'none',
              color: line.type === 'add' ? '#74c99188' : '#555',
            }}>
              {line.newNum ?? ''}
            </div>
            <div style={{
              padding: '1px 6px',
              background: line.type === 'add' ? '#74c99122' : line.type === 'del' ? '#c74e3922' : undefined,
              color: line.type === 'add' ? 'var(--app-diff-addition-foreground)'
                   : line.type === 'del' ? 'var(--app-diff-deletion-foreground)'
                   : 'var(--app-secondary-foreground)',
              whiteSpace: 'pre',
            }}>
              {line.type === 'add' ? '+' : line.type === 'del' ? '-' : ' '}{line.text}
            </div>
          </React.Fragment>
        ))}
      </div>
    </div>
  );
}
