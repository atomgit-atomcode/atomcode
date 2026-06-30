import React from 'react';

export interface DiffLine {
  type: 'add' | 'del' | 'ctx' | 'hunk' | 'meta';
  oldNum?: number;
  newNum?: number;
  text: string;
}

interface DiffViewProps {
  content: string;
}

function isDiffMetaLine(line: string): boolean {
  return (
    line.startsWith('diff --git ') ||
    line.startsWith('index ') ||
    line.startsWith('new file mode ') ||
    line.startsWith('deleted file mode ') ||
    line.startsWith('old mode ') ||
    line.startsWith('new mode ') ||
    line.startsWith('similarity index ') ||
    line.startsWith('dissimilarity index ') ||
    line.startsWith('rename from ') ||
    line.startsWith('rename to ') ||
    line.startsWith('copy from ') ||
    line.startsWith('copy to ') ||
    line.startsWith('--- ') ||
    line.startsWith('+++ ')
  );
}

function isDiffSentinelLine(line: string): boolean {
  return /^(diff|patch)$/i.test(line.trim());
}

export function parseDiff(raw: string): DiffLine[] {
  const lines: DiffLine[] = [];
  let oldNum = 0;
  let newNum = 0;
  for (const line of raw.split('\n')) {
    if (isDiffSentinelLine(line)) {
      continue;
    }
    if (line.startsWith('@@')) {
      const m = line.match(/@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (m) {
        oldNum = parseInt(m[1], 10);
        newNum = parseInt(m[2], 10);
      }
      lines.push({ type: 'hunk', text: line });
      continue;
    }
    if (isDiffMetaLine(line)) {
      lines.push({ type: 'meta', text: line });
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

function markerForType(type: DiffLine['type']): string {
  if (type === 'add') return '+';
  if (type === 'del') return '-';
  return ' ';
}

export function DiffView({ content }: DiffViewProps) {
  const lines = parseDiff(content);
  if (lines.length === 0) return null;

  return (
    <div className="diff-view">
      {lines.map((line, i) => {
        const marker = markerForType(line.type);
        return (
          <div className={`diff-row diff-row-${line.type}`} key={i}>
            <span className="diff-line-number old">{line.oldNum ?? ''}</span>
            <span className="diff-line-number new">{line.newNum ?? ''}</span>
            <span className="diff-marker">{line.type === 'hunk' || line.type === 'meta' ? '' : marker}</span>
            <span className="diff-line-content">{line.text || ' '}</span>
          </div>
        );
      })}
    </div>
  );
}
