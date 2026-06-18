import React from 'react';

interface Props { original: string; modified: string }

export const DiffView: React.FC<Props> = ({ original, modified }) => {
  const lines = computeDiff(original, modified);
  return (
    <div className="diff-view">
      {lines.map((l, i) => (
        <div key={i} className={`diff-line diff-${l.type}`}>
          <span className="line-num">{l.lineNum}</span>
          <span className="line-content">{l.content}</span>
        </div>
      ))}
    </div>
  );
};

function computeDiff(a: string, b: string): Array<{ type: string; lineNum: string; content: string }> {
  const aLines = a.split('\n'), bLines = b.split('\n');
  const result: Array<{ type: string; lineNum: string; content: string }> = [];
  for (let i = 0; i < Math.max(aLines.length, bLines.length); i++) {
    if (i < aLines.length && i < bLines.length && aLines[i] === bLines[i]) {
      result.push({ type: 'context', lineNum: `${i + 1}`, content: aLines[i] });
    } else {
      if (i < aLines.length) result.push({ type: 'removed', lineNum: '-', content: aLines[i] });
      if (i < bLines.length) result.push({ type: 'added', lineNum: '+', content: bLines[i] });
    }
  }
  return result;
}
