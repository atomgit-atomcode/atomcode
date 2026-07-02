import * as vscode from 'vscode';
import * as path from 'path';

export interface EditorContext {
  filePath?: string;
  fileName?: string;
  language?: string;
  selection?: string;
  startLine?: number;
  endLine?: number;
}

export function getEditorContext(): EditorContext {
  const editor = vscode.window.activeTextEditor;
  if (!editor) return {};

  const selection = editor.selection;
  const hasSelection = !selection.isEmpty;

  return {
    filePath: editor.document.uri.fsPath,
    fileName: path.basename(editor.document.uri.fsPath),
    language: editor.document.languageId,
    selection: hasSelection ? editor.document.getText(selection) : undefined,
    startLine: hasSelection ? selection.start.line + 1 : undefined,
    endLine: hasSelection ? selection.end.line + 1 : undefined,
  };
}

function isChineseLocale(locale?: string) {
  return locale?.toLowerCase().startsWith('zh') ?? false;
}

export function buildContextualPrompt(action: string, context: EditorContext, locale?: string): string {
  if (!context.selection || !context.fileName) {
    return action;
  }

  const zh = isChineseLocale(locale);
  const location = context.startLine === context.endLine
    ? (zh ? `第 ${context.startLine} 行` : `line ${context.startLine}`)
    : (zh ? `第 ${context.startLine}-${context.endLine} 行` : `lines ${context.startLine}-${context.endLine}`);
  const fileLabel = zh ? '文件' : 'File';
  const selectionLabel = zh ? '选中代码' : 'Selected code';
  const colon = zh ? '：' : ':';
  const filePrefix = zh
    ? `${fileLabel}${colon}${context.fileName} (${context.language || 'unknown'})`
    : `${fileLabel}${colon} ${context.fileName} (${context.language || 'unknown'})`;
  const selectionPrefix = zh
    ? `${selectionLabel}（${location}）${colon}`
    : `${selectionLabel} (${location})${colon}`;

  return `${filePrefix}\n${selectionPrefix}\n\`\`\`${context.language || ''}\n${context.selection}\n\`\`\`\n\n${action}`;
}
