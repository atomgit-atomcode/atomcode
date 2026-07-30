export interface OpenFileSelection {
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
}

function positiveSafeInteger(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 1
    ? value
    : undefined;
}

export function parseOpenFileSelection(message: {
  startLine?: unknown;
  startColumn?: unknown;
  endLine?: unknown;
  endColumn?: unknown;
}): OpenFileSelection | undefined {
  if (message.startLine === undefined) return undefined;

  const startLine = positiveSafeInteger(message.startLine);
  const startColumn = message.startColumn === undefined
    ? 1
    : positiveSafeInteger(message.startColumn);
  const endLine = message.endLine === undefined
    ? startLine
    : positiveSafeInteger(message.endLine);
  const endColumn = message.endColumn === undefined
    ? startColumn
    : positiveSafeInteger(message.endColumn);

  if (startLine === undefined || startColumn === undefined
    || endLine === undefined || endColumn === undefined) {
    return undefined;
  }
  if (endLine < startLine || (endLine === startLine && endColumn < startColumn)) {
    return undefined;
  }
  return { startLine, startColumn, endLine, endColumn };
}
